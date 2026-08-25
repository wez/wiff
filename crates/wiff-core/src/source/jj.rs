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

/// The context wiff asks jj for around each hunk.  Matches the git constant so
/// diffs from both SCMs have the same neighborhood width.
const JJ_CONTEXT_LINES: u32 = 3000;

/// The SHA1 of the empty git tree.  jj always uses a git SHA1 object store, so
/// this constant is the universal empty-tree base for any jj repository.
const JJ_EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// jj's virtual root commit ID (all zeros).  In jj 0.42+, `trunk()` resolves
/// to this when no trunk is configured rather than returning empty; see
/// <https://jj-vcs.github.io/jj/latest/revsets/#built-in-functions> for the
/// `trunk()` semantics.  Any revset result equal to this should be treated as
/// "not meaningfully resolved".
const JJ_ROOT_COMMIT: &str = "0000000000000000000000000000000000000000";

/// Revision resolution, diff production, and forge operations against a jj
/// repository.  Holds the subprocess plumbing that both the base-ruleset
/// resolver and a capture run through.
#[derive(Debug, Clone)]
pub struct JjRepo {
    repo_root: PathBuf,
}

impl JjRepo {
    /// A handle to the jj repository rooted at `repo_root`.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// A `GitRepo` pointed at the repo root, for forge operations (fetch, pin,
    /// publish) that must speak git's ref and network protocol.  Only valid for
    /// colocated jj+git workspaces where `.git` exists alongside `.jj`.  Returns
    /// an error for non-colocated workspaces.
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
    /// success.  jj is started in a fresh session so it has no controlling
    /// terminal and cannot block on a credential prompt.
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
    /// first.  A nonzero exit that represents "nothing matched" (typically exit
    /// 1 from `jj log -r <empty revset>`) yields `Ok(None)`; any other
    /// nonzero exit is a genuine jj failure.
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

    /// Resolve `revset` to a single commit hash via `jj log`.
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

    /// The first local (non-tracking) bookmark on `@`, or `None` when `@` has
    /// no local bookmarks.
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

/// The branch state of the jj workspace at `repo_root`, reported as `On` with
/// a pseudo-ref when `@` has a local bookmark, `Detached` when it has none,
/// and `Unknown` when jj cannot be reached or gives an unreadable answer.
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
        // jj's `trunk()` revset resolves the configured trunk branch; see
        // https://jj-vcs.github.io/jj/latest/revsets/#built-in-functions.  In
        // jj 0.42+, it always resolves but defaults to the all-zeros root
        // commit when unconfigured rather than returning empty.  Treat the root
        // commit as "not configured" and fall back to well-known bookmark names.
        if let Some(rev) = self.resolve_revset("trunk()").await? {
            if rev.as_str() != JJ_ROOT_COMMIT {
                return Ok(Some(rev));
            }
        }
        for name in ["main", "master"] {
            if let Some(rev) = self.resolve_revset(name).await? {
                return Ok(Some(rev));
            }
        }
        Ok(None)
    }

    async fn upstream(&self) -> Result<Option<RevisionId>> {
        // Resolve the remote-tracking bookmark for the current change. jj
        // expresses this as `<bookmark>@<remote>`; without a local bookmark on
        // `@` there is no upstream to resolve.
        let Some(bookmark) = self.current_bookmark().await? else {
            return Ok(None);
        };
        for remote in ["origin", "upstream"] {
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
            // A root commit has no parent; fall back to the empty tree.
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
        Ok(Some(RevisionId(JJ_EMPTY_TREE_SHA1.to_string())))
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
        // jj always commits the working copy into `@`; "clean" means `@` has
        // no changes relative to its parent, which `jj diff` shows as empty.
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
        // Validate colocated repo early so the user gets a clear error rather
        // than a cryptic "jj git push failed" when there is no git backend.
        let git = self.git_repo_for_forge()?;
        drop(git);
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

/// A jj diff of a reviewed range: a base ruleset and a tip rule that resolve
/// to concrete commits, then diffed.  A working-tree tip diffs the resolved
/// base against the uncommitted working copy (`@`); a ref, change-id, or
/// pinned tip diffs the base against the resolved tip commit.
#[derive(Debug, Clone)]
pub struct JjSource {
    repo: JjRepo,
    base: BaseRuleset,
    tip: TipRule,
}

impl JjSource {
    /// A source reviewing the working copy (`@`) against `base`.
    pub fn working_copy(repo_root: impl Into<PathBuf>, base: BaseRuleset) -> Self {
        Self {
            repo: JjRepo::new(repo_root),
            base,
            tip: TipRule::WorkingCopy,
        }
    }

    /// A source reviewing `base` against the commit a ref, change-id, or
    /// pinned `tip` resolves to.
    pub fn revision(repo_root: impl Into<PathBuf>, base: BaseRuleset, tip: TipRule) -> Self {
        Self {
            repo: JjRepo::new(repo_root),
            base,
            tip,
        }
    }

    /// Build a source reviewing `change` against `base`.  A `change` that
    /// names a local bookmark is tracked as a [`Ref`](TipRule::Ref) tip; a
    /// jj change ID (all-lowercase letters) is held as a
    /// [`ChangeId`](TipRule::ChangeId); anything else is resolved to a commit
    /// and held as a [`Pinned`](TipRule::Pinned) tip.  Errors when `change`
    /// resolves to no commit.
    pub async fn change(
        repo_root: impl Into<PathBuf>,
        base: BaseRuleset,
        change: String,
    ) -> Result<Self> {
        let repo = JjRepo::new(repo_root);
        let tip = if looks_like_change_id(&change) {
            // A jj change ID tracks the logical change across rewrites.
            TipRule::ChangeId {
                id: ChangeId(change),
            }
        } else if repo
            .resolve_revset(&format!("exact:\"{}\"", change.replace('"', "\\\"")))
            .await?
            .is_some()
        {
            // An exact bookmark name re-resolves on every refresh.
            TipRule::Ref { name: change }
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

    /// The base ruleset pinning the review at the parent of the working copy
    /// (`@-`).  In jj all working-copy changes live in `@` itself, so diffing
    /// `@-` → `@` shows exactly what `@` contributes — the jj equivalent of
    /// `git diff HEAD`.  Falls back to the empty tree for a repo whose first
    /// commit has no parent.
    pub async fn pinned_base_at_head(repo_root: impl Into<PathBuf>) -> Result<BaseRuleset> {
        let repo = JjRepo::new(repo_root);
        Ok(match repo.resolve_revset("@-").await? {
            Some(parent) if parent.as_str() != JJ_ROOT_COMMIT => BaseRuleset::pinned(&parent),
            _ => BaseRuleset::empty(),
        })
    }

    /// Resolve the tip rule to the commit the diff runs up to.  A working-copy
    /// tip resolves `@`; a change ID resolves its current commit through jj's
    /// native addressing; a ref re-resolves its bookmark; a pinned tip
    /// verifies the commit still exists.
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
/// uppercase).  Commit hashes are hex (digits + a–f), bookmark names typically
/// contain slashes or digits, and revsets contain operators.
fn looks_like_change_id(s: &str) -> bool {
    // jj change IDs are random lowercase-letter strings; common branch names
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
    /// parent of `repo` so jj writes its user config outside the git repo,
    /// preventing it from appearing in commits.
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
    /// since the repo doesn't exist yet.  HOME is set to the parent of `dir` so
    /// jj writes its user config outside the git repo.
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

    /// Blank the variable `index <old>..<new>` blob hashes so the captured
    /// patch can be asserted whole.
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
        assert!(!looks_like_change_id("abc123")); // contains digit
        assert!(!looks_like_change_id("main")); // bookmark: looks like change id, but short
        assert!(!looks_like_change_id("")); // empty
        assert!(!looks_like_change_id("refs/heads/main")); // contains slash
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
        // The base is pinned at @- (parent), so base_revision is that commit.
        assert_eq!(captured.base_revision, Some(parent_sha));
        assert!(!captured.base_tip_relative);
        assert!(matches!(captured.source, SourceKind::Scm(_)));
        if let SourceKind::Scm(src) = &captured.source {
            assert_eq!(src.scm, ScmType::Jujutsu);
            assert!(matches!(src.tip, TipRule::WorkingCopy));
            // @ has no local bookmark, so branch_hint is None.
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
        // parent(@) on jj's first commit resolves to jj's virtual root commit
        // (all zeros), which represents the empty state before any commits.
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
        // In jj 0.42+, trunk() resolves but defaults to the root commit (all
        // zeros) when unconfigured. Verify that so we know the fallback path
        // is exercised: trunk() resolves to root → ignored → main bookmark found.
        let trunk_revset = jj_repo.resolve_revset("trunk()").await.expect("revset");
        assert_eq!(
            trunk_revset,
            Some(RevisionId(JJ_ROOT_COMMIT.to_string())),
            "trunk() should resolve to root commit (all zeros) when unconfigured"
        );
        let trunk = jj_repo.trunk().await.expect("trunk");
        assert_eq!(
            trunk,
            Some(main_sha),
            "trunk should resolve via 'main' bookmark"
        );
    }

    #[tokio::test]
    async fn empty_returns_the_well_known_sha1() {
        let repo = tempfile::tempdir().expect("tempdir");
        jj_init(repo.path());
        let jj_repo = JjRepo::new(repo.path());
        let empty = jj_repo.empty().await.expect("empty");
        assert_eq!(empty, Some(RevisionId(JJ_EMPTY_TREE_SHA1.to_string())));
    }
}
