//! A git diff of a repository as a [`DiffSource`].

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tracing::trace;

use crate::base_resolve::{RevisionResolver, resolve_base};
use crate::base_ruleset::{BaseRuleset, parse_ruleset};
use crate::error::{Error, Result};
use crate::identity::ScmType;
use crate::record::{RevisionId, ScmSource, SourceKind, TipRule};
use crate::session_id::SessionId;
use crate::source::{
    CapturedDiff, DiffSource, FetchSource, HeadBranch, Remote, ScmRepo, TrackingBranch,
};

/// The context wiff asks git for around each hunk. A large window means a hunk
/// holds most or all of its file, so highlighting and rebasing have more to work
/// with while the diff stays the single artifact.
const GIT_CONTEXT_LINES: u32 = 3000;

/// The environment for a git subcommand: an alternate index file, and a
/// writable scratch object directory paired with the repo's real objects as an
/// alternate. The scratch directory lets a capture that must write objects (an
/// intent-to-add of untracked files) run against a `.git` mounted read-only,
/// with the redirected writes discarded when the capture completes.
#[derive(Default, Clone, Copy)]
struct GitEnv<'a> {
    /// The `GIT_INDEX_FILE` git operates against, replacing the repo's real one.
    index: Option<&'a Path>,
    /// The `GIT_OBJECT_DIRECTORY` git writes new objects into.
    scratch_objects: Option<&'a Path>,
    /// The repo's real object directory, offered via
    /// `GIT_ALTERNATE_OBJECT_DIRECTORIES` so reads still find existing objects
    /// when writes are redirected to [`Self::scratch_objects`].
    real_objects: Option<&'a Path>,
}

/// Revision resolution and diff production against a git repository. Holds the
/// git subprocess plumbing that both the base-ruleset resolver and a capture run
/// through, so a [`GitSource`] and a bare resolution both speak to git the same
/// way (a laundered process with no controlling terminal, an alternate index and
/// object directory when a capture must write).
#[derive(Debug, Clone)]
pub struct GitRepo {
    repo_root: PathBuf,
}

impl GitRepo {
    /// A handle to the git repository rooted at `repo_root`.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// Copy the repo's real index into a temporary file. A repository without an
    /// index yet (freshly initialized, no commits) gets a valid empty index, so
    /// every file shows as an addition.
    async fn seed_temp_index(&self) -> Result<NamedTempFile> {
        let temp = NamedTempFile::new().map_err(|source| {
            Error::Source(format!("could not create a temporary index: {source}"))
        })?;
        let real = self.real_index_path().await?;
        match tokio::fs::read(&real).await {
            Ok(bytes) => {
                let handle = temp.as_file().try_clone().map_err(|source| {
                    Error::Source(format!("could not open the temporary index: {source}"))
                })?;
                let mut file = tokio::fs::File::from_std(handle);
                file.write_all(&bytes).await.map_err(|source| {
                    Error::Source(format!("could not write the temporary index: {source}"))
                })?;
                file.flush().await.map_err(|source| {
                    Error::Source(format!("could not write the temporary index: {source}"))
                })?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // git rejects a zero-length GIT_INDEX_FILE ("index file smaller
                // than expected"), so write a valid empty index into the
                // throwaway before anything else reads or updates it.
                self.git(
                    ["read-tree", "--empty"],
                    GitEnv {
                        index: Some(temp.path()),
                        ..GitEnv::default()
                    },
                )
                .await?;
            }
            Err(source) => {
                return Err(Error::Source(format!(
                    "could not read the git index: {source}"
                )));
            }
        }
        Ok(temp)
    }

    /// The path to the repo's real index file.
    async fn real_index_path(&self) -> Result<PathBuf> {
        self.git_path("index").await
    }

    /// The path to the repo's real object directory.
    async fn real_objects_path(&self) -> Result<PathBuf> {
        self.git_path("objects").await
    }

    /// Resolve `name` under the repo's git directory to an absolute path.
    async fn git_path(&self, name: &str) -> Result<PathBuf> {
        let output = self
            .git(["rev-parse", "--git-path", name], GitEnv::default())
            .await?;
        let text = String::from_utf8(output.stdout).map_err(|source| {
            Error::Source(format!("git rev-parse was not valid UTF-8: {source}"))
        })?;
        let path = Path::new(text.trim());
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.repo_root.join(path)
        })
    }

    /// Record every untracked, non-ignored file as intent-to-add in `index`, so
    /// it appears in the diff as a new file with its full content.
    async fn add_untracked_files_to_temp_index(&self, env: GitEnv<'_>) -> Result<()> {
        let output = self
            .git(
                ["ls-files", "--others", "--exclude-standard", "-z"],
                GitEnv::default(),
            )
            .await?;
        let untracked: Vec<OsString> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| OsStr::from_bytes(path).to_owned())
            .collect();
        if untracked.is_empty() {
            return Ok(());
        }
        let mut args: Vec<OsString> = vec!["add".into(), "--intent-to-add".into(), "--".into()];
        args.extend(untracked);
        self.git(args, env).await?;
        Ok(())
    }

    /// Run `git diff` with expanded context and the given trailing arguments
    /// (the base, and for a two-commit diff the tip), under `env`.
    async fn diff(&self, trailing: &[OsString], env: GitEnv<'_>) -> Result<String> {
        let mut args: Vec<OsString> = vec![
            "diff".into(),
            format!("--unified={GIT_CONTEXT_LINES}").into(),
        ];
        args.extend_from_slice(trailing);
        let output = self.git(args, env).await?;
        String::from_utf8(output.stdout)
            .map_err(|source| Error::Source(format!("git diff was not valid UTF-8: {source}")))
    }

    /// Run a git subcommand under the repo and return its output on success.
    /// `env` redirects git's index and object storage away from the repo's real
    /// ones when set.
    ///
    /// git is started in a fresh session so it has no controlling terminal.
    /// stdin on /dev/null is not enough on its own: git opens /dev/tty directly
    /// to prompt for credentials, which would hang us (and fight the TUI for the
    /// terminal). Without a controlling terminal that open fails, so a diff
    /// needing credentials fails cleanly instead. setsid(2) is async-signal-safe,
    /// so it is safe to call in pre_exec.
    async fn git<I, S>(&self, args: I, env: GitEnv<'_>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let output = self.spawn(&args, env).await?;
        if !output.status.success() {
            let subcommand = args
                .first()
                .map(|arg| arg.to_string_lossy().into_owned())
                .unwrap_or_default();
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Repo(format!(
                "git {subcommand} failed ({}): {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(output)
    }

    /// Run a git subcommand and return its raw output whatever the exit status,
    /// failing only when the process could not be started. A nonzero exit is a
    /// result for the caller to interpret.
    async fn spawn(&self, args: &[OsString], env: GitEnv<'_>) -> Result<Output> {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.repo_root).args(args);
        command.stdin(Stdio::null());
        if let Some(index) = env.index {
            command.env("GIT_INDEX_FILE", index);
        }
        if let Some(objects) = env.scratch_objects {
            command.env("GIT_OBJECT_DIRECTORY", objects);
        }
        if let Some(alternate) = env.real_objects {
            command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate);
        }
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
        trace!(repo = %self.repo_root.display(), args = ?rendered, "running git");
        let output = tokio::process::Command::from(command)
            .output()
            .await
            .map_err(|source| Error::Repo(format!("could not run git: {source}")))?;
        trace!(
            args = ?rendered,
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "git finished"
        );
        Ok(output)
    }

    /// Run a git query that prints a single revision, returning it trimmed.
    /// Exit code 1 is git's "named nothing" for the queries resolution runs (an
    /// unknown ref under `rev-parse --verify --quiet`, no common ancestor under
    /// `merge-base`) and yields `None`; any other nonzero exit is a genuine git
    /// failure, reported with its stderr rather than mistaken for a
    /// fall-through. Non-UTF-8 output is a hard error.
    async fn rev_query(&self, args: &[OsString]) -> Result<Option<RevisionId>> {
        let output = self.spawn(args, GitEnv::default()).await?;
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
                    "git {subcommand} failed ({}): {}",
                    output.status,
                    stderr.trim()
                )));
            }
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|source| Error::Repo(format!("git printed a non-UTF-8 revision: {source}")))?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(RevisionId(trimmed.to_string())))
    }

    /// Resolve a revision spec to a commit through `rev-parse`, peeling to a
    /// commit. `--end-of-options` keeps a spec beginning with `-` from being
    /// read as a git option, and `--verify --quiet` makes an unresolvable spec
    /// exit 1, which [`rev_query`](Self::rev_query) reports as `None`.
    async fn resolve_commit(&self, spec: &str) -> Result<Option<RevisionId>> {
        self.rev_query(&[
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "--end-of-options".into(),
            format!("{spec}^{{commit}}").into(),
        ])
        .await
    }

    /// The full ref name `spec` names (`refs/heads/...`, `refs/tags/...`), or
    /// `None` when it does not name a ref: a bare revision, a detached `HEAD`,
    /// or a spec that resolves to nothing at all.
    ///
    /// `rev-parse --symbolic-full-name` prints empty for a bare revision, the
    /// literal `HEAD` for a detached head, and exits nonzero for an unresolvable
    /// spec; only an answer under `refs/` is a ref whose name is worth keeping.
    /// The full name is returned rather than the spec so a later re-resolution is
    /// unambiguous when a tag and a branch share a short name.
    async fn symbolic_ref(&self, spec: &str) -> Result<Option<String>> {
        let output = self
            .spawn(
                &[
                    "rev-parse".into(),
                    "--symbolic-full-name".into(),
                    spec.into(),
                ],
                GitEnv::default(),
            )
            .await?;
        ref_name_from_symbolic(output.status.success(), &output.stdout)
    }

    /// The branch state of the repository: the branch HEAD is on (full ref name,
    /// even when unborn), a detached head, or an unknown state when HEAD is
    /// neither. The async counterpart to the free [`head_branch`] function;
    /// errors only when git could not be run at all.
    async fn head_branch(&self) -> Result<HeadBranch> {
        let output = self
            .spawn(
                &["symbolic-ref".into(), "--quiet".into(), "HEAD".into()],
                GitEnv::default(),
            )
            .await?;
        Ok(classify_head(output.status.code(), &output.stdout))
    }
}

/// Interpret the output of `rev-parse --symbolic-full-name <spec>`: the full ref
/// name when git named one under `refs/`, or `None` for a bare revision, a
/// detached head (git echoes the literal `HEAD`), or an unresolvable spec (git
/// exits nonzero).
fn ref_name_from_symbolic(success: bool, stdout: &[u8]) -> Result<Option<String>> {
    if !success {
        return Ok(None);
    }
    let text = std::str::from_utf8(stdout)
        .map_err(|source| Error::Repo(format!("git rev-parse was not valid UTF-8: {source}")))?;
    let name = text.trim();
    Ok(name.starts_with("refs/").then(|| name.to_string()))
}

/// Classify HEAD from the exit status and stdout of `git symbolic-ref --quiet
/// HEAD`. git names the branch and exits 0 even when it is unborn (a repository
/// with no commits, where HEAD points at a branch that has none), unlike
/// `rev-parse HEAD`, which fails there. It exits 1 for a detached head, where
/// HEAD is a commit but not a symbolic ref, and with any other code when this is
/// not a repository git can read.
fn classify_head(code: Option<i32>, stdout: &[u8]) -> HeadBranch {
    match code {
        Some(0) => match std::str::from_utf8(stdout)
            .ok()
            .map(str::trim)
            .filter(|name| name.starts_with("refs/"))
        {
            Some(name) => HeadBranch::On(name.to_string()),
            None => HeadBranch::Unknown,
        },
        Some(1) => HeadBranch::Detached,
        _ => HeadBranch::Unknown,
    }
}

/// The branch state of the git repository at `repo_root`: the branch it is on
/// (full ref name, `refs/heads/...`), a detached head, or an unknown state when
/// git cannot be reached or gives an answer that cannot be read. The sync
/// counterpart to [`GitRepo::head_branch`] for paths that run outside an async
/// context.
pub fn head_branch(repo_root: &Path) -> HeadBranch {
    let Ok(output) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .stdin(Stdio::null())
        .output()
    else {
        return HeadBranch::Unknown;
    };
    classify_head(output.status.code(), &output.stdout)
}

#[async_trait]
impl RevisionResolver for GitRepo {
    fn scm(&self) -> ScmType {
        ScmType::Git
    }

    async fn resolve_ref(&self, name: &str) -> Result<Option<RevisionId>> {
        self.resolve_commit(name).await
    }

    async fn trunk(&self) -> Result<Option<RevisionId>> {
        // The default branch is the remote's HEAD, falling back to a local main
        // or master when no remote HEAD is set (a freshly initialized repo, or a
        // clone before git records origin's head).
        for candidate in ["origin/HEAD", "main", "master"] {
            if let Some(rev) = self.resolve_commit(candidate).await? {
                return Ok(Some(rev));
            }
        }
        Ok(None)
    }

    async fn upstream(&self) -> Result<Option<RevisionId>> {
        self.resolve_commit("@{upstream}").await
    }

    async fn parent(&self, rev: &RevisionId) -> Result<Option<RevisionId>> {
        // A root commit has no first parent; fall back to the empty tree.
        match self.resolve_commit(&format!("{}^1", rev.as_str())).await? {
            Some(parent) => Ok(Some(parent)),
            None => self.empty().await,
        }
    }

    async fn merge_base(&self, rev: &RevisionId, tip: &RevisionId) -> Result<Option<RevisionId>> {
        self.rev_query(&[
            "merge-base".into(),
            "--end-of-options".into(),
            rev.as_str().into(),
            tip.as_str().into(),
        ])
        .await
    }

    async fn native(&self, expr: &str) -> Result<Option<RevisionId>> {
        // An scm-native escape hatch for expressions the fixed operators cannot
        // form; the expression reaches rev-parse verbatim with no ^{commit} peel
        // appended, and --end-of-options guards a leading dash.
        self.rev_query(&[
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "--end-of-options".into(),
            expr.into(),
        ])
        .await
    }

    async fn empty(&self) -> Result<Option<RevisionId>> {
        // Hashing an empty tree yields the repo's empty-tree object name under
        // whatever hash algorithm it uses, rather than assuming the sha1
        // constant. Without -w the object is never written; git special-cases
        // the empty tree so it serves as a diff base even when absent from the
        // object store.
        self.rev_query(&[
            "hash-object".into(),
            "-t".into(),
            "tree".into(),
            "/dev/null".into(),
        ])
        .await
    }
}

/// Which of a session's two pin refs to address: the reviewed head, or the
/// base it is compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PinSlot {
    Head,
    Base,
}

impl PinSlot {
    fn as_str(self) -> &'static str {
        match self {
            PinSlot::Head => "head",
            PinSlot::Base => "base",
        }
    }
}

/// The prefix under which wiff owns all of `session`'s refs, kept apart from
/// `refs/heads/` so a pin can never name or clobber a user's own branch.
fn session_ref_prefix(session: SessionId) -> String {
    format!("refs/wiff/{session}/")
}

/// Build the ref that pins one slot of `session`, under wiff's own namespace.
fn pin_ref(session: SessionId, slot: PinSlot) -> String {
    format!("{}{}", session_ref_prefix(session), slot.as_str())
}

impl GitRepo {
    /// Create or move `refname` to `commit`. Unconditional (no expected old
    /// value) so re-pinning the same slot stays idempotent.
    async fn update_ref(&self, refname: &str, commit: &RevisionId) -> Result<()> {
        self.git(
            [
                "update-ref".into(),
                OsString::from(refname),
                OsString::from(commit.as_str()),
            ],
            GitEnv::default(),
        )
        .await?;
        Ok(())
    }

    /// Delete `refname`, tolerating its absence. Deletes by name without peeling
    /// to a commit, so it removes even a ref whose target object is missing.
    async fn delete_ref(&self, refname: &str) -> Result<()> {
        self.git(
            ["update-ref".into(), "-d".into(), OsString::from(refname)],
            GitEnv::default(),
        )
        .await?;
        Ok(())
    }

    /// The local branch name `HEAD` is on, or `None` when the head is detached.
    /// Names an unborn branch (a repository with no commits) like any other.
    async fn current_branch_name(&self) -> Result<Option<String>> {
        Ok(match self.head_branch().await? {
            HeadBranch::On(full) => full.strip_prefix("refs/heads/").map(str::to_string),
            HeadBranch::Detached | HeadBranch::Unknown => None,
        })
    }

    async fn config_is_set(&self, key: &str) -> Result<bool> {
        Ok(self.config_get(key).await?.is_some())
    }

    /// The value git config holds for `key`, or `None` when the key is unset.
    /// `git config --get` exits 1 for a key that is simply absent; any other
    /// nonzero exit (a multi-valued key, an unreadable or corrupt config) is a
    /// genuine failure rather than "unset".
    async fn config_get(&self, key: &str) -> Result<Option<String>> {
        let output = self
            .spawn(
                &["config".into(), "--get".into(), OsString::from(key)],
                GitEnv::default(),
            )
            .await?;
        match output.status.code() {
            Some(0) => {
                let text = String::from_utf8(output.stdout).map_err(|source| {
                    Error::Repo(format!("git printed a non-UTF-8 config value: {source}"))
                })?;
                Ok(Some(text.trim().to_string()))
            }
            Some(1) => Ok(None),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(Error::Repo(format!(
                    "git config --get {key} failed ({}): {}",
                    output.status,
                    stderr.trim()
                )))
            }
        }
    }

    /// Record `branch_name` as tracking `branch` on `remote`, unless it already
    /// has an upstream, which is left as the user configured it. The config is
    /// written directly rather than through `branch --set-upstream-to`, which
    /// would need a remote-tracking ref to already exist.
    async fn set_upstream_if_unset(
        &self,
        branch_name: &str,
        remote: &str,
        branch: &str,
    ) -> Result<()> {
        // An upstream is the pair `.remote` + `.merge`; treat the branch as
        // configured only when both are present. A half-configured branch (one
        // key hand-edited or left over) is completed here rather than left
        // unable to reach the published branch with a plain `git push`.
        let has_remote = self
            .config_is_set(&format!("branch.{branch_name}.remote"))
            .await?;
        let has_merge = self
            .config_is_set(&format!("branch.{branch_name}.merge"))
            .await?;
        if has_remote && has_merge {
            return Ok(());
        }
        self.git(
            [
                "config".into(),
                OsString::from(format!("branch.{branch_name}.remote")),
                OsString::from(remote),
            ],
            GitEnv::default(),
        )
        .await?;
        self.git(
            [
                "config".into(),
                OsString::from(format!("branch.{branch_name}.merge")),
                OsString::from(format!("refs/heads/{branch}")),
            ],
            GitEnv::default(),
        )
        .await?;
        Ok(())
    }
}

/// What the forge's promised commit means relative to the ref just fetched, and
/// so which commit ends up pinned.
enum FetchExpect {
    /// The fetched ref must resolve to the promised commit, and that commit is
    /// pinned. A pull request's head lives at an immutable `refs/pull/<n>/head`,
    /// so a mismatch means the forge and the fetch disagree.
    Exact,
    /// The promised commit must be reachable from the fetched ref, and that
    /// commit is pinned rather than the ref's tip. A pull request's target
    /// branch keeps moving, and the forge reports the tip as of its last sync,
    /// which the live branch has usually advanced past; the older commit is
    /// still on the branch and is what the review's base is anchored to.
    Reachable,
}

impl GitRepo {
    /// Fetch `source` into a per-session scratch ref, check the promised commit
    /// against `expect`, then pin the resulting commit under `slot` and return
    /// it.
    async fn fetch_and_pin(
        &self,
        source: &FetchSource,
        session: SessionId,
        slot: PinSlot,
        expect: FetchExpect,
    ) -> Result<RevisionId> {
        let FetchSource::Git {
            url,
            git_ref,
            commit,
        } = source;
        let pin = pin_ref(session, slot);
        // Fetch into a per-session scratch ref rather than the pin or the shared
        // FETCH_HEAD: a scratch ref keeps concurrent fetches in the same repo
        // from observing one another's commit, and it keeps a re-fetch that
        // turns out inconsistent from destroying a good commit an earlier
        // successful fetch pinned. A slot-specific name keeps a session's head
        // and base fetches from colliding. The leading `+` overwrites any
        // scratch ref an interrupted fetch left behind.
        let scratch = format!("{}incoming-{}", session_ref_prefix(session), slot.as_str());
        self.git(
            [
                "fetch".into(),
                "--no-tags".into(),
                OsString::from(url),
                OsString::from(format!("+{git_ref}:{scratch}")),
            ],
            GitEnv::default(),
        )
        .await?;
        let outcome = async {
            // Resolve to a full object name before comparing: the forge may
            // report an abbreviated or differently-cased hash, and the fetched
            // commit is now present under the scratch ref to resolve against.
            let tip = self
                .resolve_commit(&scratch)
                .await?
                .ok_or_else(|| Error::Repo("git fetch left no ref to resolve".to_string()))?;
            let pinned = match expect {
                FetchExpect::Exact => {
                    if self.resolve_commit(commit.as_str()).await? != Some(tip.clone()) {
                        return Err(Error::Repo(format!(
                            "fetched {git_ref} resolved to {tip}, but the forge reported {commit}"
                        )));
                    }
                    tip
                }
                FetchExpect::Reachable => {
                    // The promised commit is on the fetched branch when it is
                    // present and merge-base with the tip is the commit itself,
                    // i.e. it is an ancestor of the tip.
                    let want = self.resolve_commit(commit.as_str()).await?;
                    let reachable = match &want {
                        Some(want) => self.merge_base(want, &tip).await? == Some(want.clone()),
                        None => false,
                    };
                    if !reachable {
                        return Err(Error::Repo(format!(
                            "the pull request's base {commit} is not on {git_ref} fetched from \
                             {url}; the target branch may have been rewritten since the pull \
                             request was last synced"
                        )));
                    }
                    commit.clone()
                }
            };
            // Promote the resulting commit to the pin only now, so a bad fetch
            // never disturbs a pin from an earlier good one.
            self.update_ref(&pin, &pinned).await?;
            Ok(pinned)
        }
        .await;
        // Drop the scratch ref whether or not validation passed; the pin now
        // holds the verified commit on success and is untouched on failure.
        let _ = self.delete_ref(&scratch).await;
        outcome
    }

    /// The `url.<base>.insteadOf` rewrites in effect for this repository, as
    /// (alias, base) pairs in config order. Reading them from config rather than
    /// asking git to expand each remote in turn keeps this one subprocess for
    /// the whole set, and leaves the substitution a pure function to test.
    async fn url_rewrites(&self) -> Result<Vec<(String, String)>> {
        let output = self
            .spawn(
                &[
                    "config".into(),
                    "--get-regexp".into(),
                    OsString::from(r"^url\..*\.insteadof$"),
                ],
                GitEnv::default(),
            )
            .await?;
        match output.status.code() {
            // As for remotes, exit 1 is the pattern matching nothing: a git with
            // no rewrites configured, not a failure.
            Some(1) => return Ok(Vec::new()),
            Some(0) => {}
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(Error::Repo(format!(
                    "git config --get-regexp for url rewrites failed ({}): {}",
                    output.status,
                    stderr.trim()
                )));
            }
        }
        let listing = String::from_utf8(output.stdout).map_err(|source| {
            Error::Repo(format!("git printed a non-UTF-8 url rewrite: {source}"))
        })?;
        let mut rewrites = Vec::new();
        for line in listing.lines() {
            let Some((key, alias)) = line.split_once(' ') else {
                return Err(Error::Repo(format!(
                    "git config printed an unreadable url rewrite line: {line:?}"
                )));
            };
            // The key is `url.<base>.insteadOf`, printed with the variable name
            // lowercased; a base is itself a URL prefix full of dots and colons,
            // so strip the fixed prefix and suffix rather than splitting.
            let base = key
                .strip_prefix("url.")
                .and_then(|rest| rest.strip_suffix(".insteadof"))
                .ok_or_else(|| {
                    Error::Repo(format!(
                        "git config printed an unexpected url rewrite key: {key:?}"
                    ))
                })?;
            // An empty alias would prefix-match every URL; git treats it as no
            // rewrite, so drop it here rather than rewriting everything.
            if alias.is_empty() {
                continue;
            }
            rewrites.push((alias.to_string(), base.to_string()));
        }
        Ok(rewrites)
    }
}

/// Apply git's `url.<base>.insteadOf` rewrites to a clone URL, as git itself
/// does before reaching for a remote. A remote can be configured through an
/// alias (`octo:demo.git` for `git@github.com:octo/demo.git`), whose own text
/// names neither the forge's host nor the repository; expanding it here means
/// everything downstream reads the URL git would really contact. The longest
/// matching alias wins, and among equally long ones the first in config order,
/// matching git's own choice. A URL matching no alias is its own rewrite.
fn rewrite_clone_url(clone_url: &str, rewrites: &[(String, String)]) -> String {
    let mut best: Option<(&str, &str)> = None;
    for (alias, base) in rewrites {
        if !clone_url.starts_with(alias.as_str()) {
            continue;
        }
        if best.is_none_or(|(longest, _)| alias.len() > longest.len()) {
            best = Some((alias, base));
        }
    }
    match best {
        Some((alias, base)) => format!("{base}{}", &clone_url[alias.len()..]),
        None => clone_url.to_string(),
    }
}

#[async_trait]
impl ScmRepo for GitRepo {
    async fn fetch_pinned(&self, source: &FetchSource, session: SessionId) -> Result<RevisionId> {
        self.fetch_and_pin(source, session, PinSlot::Head, FetchExpect::Exact)
            .await
    }

    async fn fetch_base(&self, source: &FetchSource, session: SessionId) -> Result<RevisionId> {
        self.fetch_and_pin(source, session, PinSlot::Base, FetchExpect::Reachable)
            .await
    }

    async fn remotes(&self) -> Result<Vec<Remote>> {
        // Read remote URLs from config rather than parsing `git remote -v`,
        // whose output pairs a fetch and a push line per remote. --get-regexp
        // prints matches in config-file order, and git fetches from a remote's
        // first `url` value, so keeping the first per name yields the fetch URL
        // and folds a multi-URL remote (git remote set-url --add) into one.
        let output = self
            .spawn(
                &[
                    "config".into(),
                    "--get-regexp".into(),
                    OsString::from(r"^remote\..*\.url$"),
                ],
                GitEnv::default(),
            )
            .await?;
        match output.status.code() {
            // git config exits 1 when the pattern matches nothing: a repo with
            // no remotes yet, not a failure.
            Some(1) => return Ok(Vec::new()),
            Some(0) => {}
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(Error::Repo(format!(
                    "git config --get-regexp for remotes failed ({}): {}",
                    output.status,
                    stderr.trim()
                )));
            }
        }
        let listing = String::from_utf8(output.stdout).map_err(|source| {
            Error::Repo(format!("git printed a non-UTF-8 remote URL: {source}"))
        })?;
        let mut remotes = Vec::new();
        for line in listing.lines() {
            let Some((key, url)) = line.split_once(' ') else {
                return Err(Error::Repo(format!(
                    "git config printed an unreadable remote line: {line:?}"
                )));
            };
            // The key is `remote.<name>.url`; a remote name may itself contain a
            // dot, so strip the fixed prefix and suffix rather than splitting.
            let name = key
                .strip_prefix("remote.")
                .and_then(|rest| rest.strip_suffix(".url"))
                .ok_or_else(|| {
                    Error::Repo(format!(
                        "git config printed an unexpected remote key: {key:?}"
                    ))
                })?;
            if remotes.iter().any(|remote: &Remote| remote.name == name) {
                continue;
            }
            remotes.push(Remote {
                name: name.to_string(),
                url: url.to_string(),
            });
        }
        let rewrites = self.url_rewrites().await?;
        for remote in &mut remotes {
            remote.url = rewrite_clone_url(&remote.url, &rewrites);
        }
        Ok(remotes)
    }

    async fn working_tree_is_clean(&self) -> Result<bool> {
        // A non-ignored untracked file counts as unclean, matching what a
        // working copy review captures: it records such files as intent-to-add
        // additions, so publishing a HEAD that omits them would send something
        // other than what was reviewed. Porcelain's default untracked mode
        // respects .gitignore, the same exclusion the capture applies.
        // --ignore-submodules=all excludes a submodule whose own state has
        // moved: that is not part of the superproject commit publishing sends.
        // Porcelain output is one line per changed path, so clean is no output.
        let output = self
            .git(
                [
                    OsString::from("status"),
                    "--porcelain".into(),
                    "--ignore-submodules=all".into(),
                ],
                GitEnv::default(),
            )
            .await?;
        let report = String::from_utf8(output.stdout)
            .map_err(|source| Error::Repo(format!("git printed a non-UTF-8 status: {source}")))?;
        Ok(report.trim().is_empty())
    }

    async fn remote_branch(&self, remote: &str, branch: &str) -> Result<Option<RevisionId>> {
        // --heads restricts the listing to branches, excluding tags. The ref
        // pattern is an fnmatch glob rather than a literal name, so each printed
        // "<sha>\trefs/heads/<name>" line is matched exactly against the wanted
        // ref: a `branch` bearing glob metacharacters must not report an
        // unrelated branch as the one asked for.
        let output = self
            .git(
                [
                    "ls-remote".into(),
                    "--heads".into(),
                    OsString::from(remote),
                    OsString::from(format!("refs/heads/{branch}")),
                ],
                GitEnv::default(),
            )
            .await?;
        let listing = String::from_utf8(output.stdout)
            .map_err(|source| Error::Repo(format!("git printed a non-UTF-8 ref: {source}")))?;
        let wanted = format!("refs/heads/{branch}");
        for line in listing.lines() {
            let Some((sha, refname)) = line.split_once('\t') else {
                return Err(Error::Repo(format!(
                    "git ls-remote printed an unreadable line: {line:?}"
                )));
            };
            if refname.trim() == wanted {
                return Ok(Some(RevisionId(sha.trim().to_string())));
            }
        }
        Ok(None)
    }

    async fn current_upstream(&self) -> Result<Option<TrackingBranch>> {
        let Some(branch_name) = self.current_branch_name().await? else {
            return Ok(None);
        };
        // An upstream is the pair `.remote` + `.merge`; a branch counts as
        // tracking only when both are present.
        let Some(remote) = self
            .config_get(&format!("branch.{branch_name}.remote"))
            .await?
        else {
            return Ok(None);
        };
        let Some(merge) = self
            .config_get(&format!("branch.{branch_name}.merge"))
            .await?
        else {
            return Ok(None);
        };
        let branch = merge
            .strip_prefix("refs/heads/")
            .unwrap_or(&merge)
            .to_string();
        Ok(Some(TrackingBranch { remote, branch }))
    }

    async fn publish_branch(&self, remote: &str, branch: &str, commit: &RevisionId) -> Result<()> {
        let branch_name = self.current_branch_name().await?.ok_or_else(|| {
            Error::Repo("cannot publish a branch from a detached head".to_string())
        })?;
        // Publishing points the checked-out branch at the pushed branch as its
        // upstream, so the reviewed commit must be that branch's tip; otherwise
        // a later plain `git push` would send a different commit than the one
        // published.
        let tip = self.resolve_commit("HEAD").await?.ok_or_else(|| {
            Error::Repo("the current branch has no commit to publish".to_string())
        })?;
        if self.resolve_commit(commit.as_str()).await? != Some(tip) {
            return Err(Error::Repo(format!(
                "cannot publish {commit}: it is not the tip of {branch_name}"
            )));
        }
        // A non-forced push: an existing remote branch that is not a
        // fast-forward of `commit` is refused rather than overwritten.
        self.git(
            [
                "push".into(),
                OsString::from(remote),
                OsString::from(format!("{}:refs/heads/{branch}", commit.as_str())),
            ],
            GitEnv::default(),
        )
        .await?;
        self.set_upstream_if_unset(&branch_name, remote, branch)
            .await
    }

    async fn remove_pins(&self, session: SessionId) -> Result<()> {
        // Delete every ref under the session's namespace, not just the head and
        // base pins: a fetch that died between writing its `incoming` scratch
        // ref and cleaning it up would otherwise orphan a commit that stays
        // resolvable forever, the leak the pins exist to avoid.
        let prefix = session_ref_prefix(session);
        let output = self
            .git(
                [
                    "for-each-ref".into(),
                    OsString::from("--format=%(refname)"),
                    OsString::from(&prefix),
                ],
                GitEnv::default(),
            )
            .await?;
        let listing = String::from_utf8(output.stdout)
            .map_err(|source| Error::Repo(format!("git printed a non-UTF-8 ref: {source}")))?;
        for refname in listing.lines() {
            self.delete_ref(refname).await?;
        }
        Ok(())
    }
}

/// A git diff of a reviewed range: a base ruleset and a tip rule that resolve to
/// concrete commits, then diffed. A working-tree or index tip diffs the resolved
/// base against the uncommitted state; a ref or pinned tip diffs the base against
/// the resolved tip commit.
#[derive(Debug, Clone)]
pub struct GitSource {
    repo: GitRepo,
    base: BaseRuleset,
    tip: TipRule,
}

impl GitSource {
    /// A source reviewing the uncommitted working copy against `base`.
    pub fn working_copy(repo_root: impl Into<PathBuf>, base: BaseRuleset) -> Self {
        Self {
            repo: GitRepo::new(repo_root),
            base,
            tip: TipRule::WorkingCopy,
        }
    }

    /// A source reviewing the staged index against `base` (`git diff --cached`).
    pub fn index(repo_root: impl Into<PathBuf>, base: BaseRuleset) -> Self {
        Self {
            repo: GitRepo::new(repo_root),
            base,
            tip: TipRule::Index,
        }
    }

    /// A source reviewing `base` against the commit a ref or pinned `tip`
    /// resolves to.
    pub fn revision(repo_root: impl Into<PathBuf>, base: BaseRuleset, tip: TipRule) -> Self {
        Self {
            repo: GitRepo::new(repo_root),
            base,
            tip,
        }
    }

    /// Build a source reviewing `change` against `base`. A `change` that names a
    /// ref is tracked as a [`Ref`](TipRule::Ref) tip under its full ref name;
    /// anything else (a bare revision, a detached `HEAD`) is held at the commit
    /// it resolves to as a [`Pinned`](TipRule::Pinned) tip. Errors when `change`
    /// resolves to no commit.
    pub async fn change(
        repo_root: impl Into<PathBuf>,
        base: BaseRuleset,
        change: String,
    ) -> Result<Self> {
        let repo = GitRepo::new(repo_root);
        let tip = match repo.symbolic_ref(&change).await? {
            Some(name) => TipRule::Ref { name },
            None => {
                let revision = repo.resolve_commit(&change).await?.ok_or_else(|| {
                    Error::Source(format!("'{change}' did not resolve to a commit"))
                })?;
                TipRule::Pinned { revision }
            }
        };
        Ok(Self { repo, base, tip })
    }

    /// The base ruleset that pins a review at the repository's current commit, or
    /// the empty tree when HEAD is unborn (a repository with no commits yet).
    pub async fn pinned_base_at_head(repo_root: impl Into<PathBuf>) -> Result<BaseRuleset> {
        let repo = GitRepo::new(repo_root);
        Ok(match repo.resolve_ref("HEAD").await? {
            Some(head) => BaseRuleset::pinned(&head),
            None => BaseRuleset::empty(),
        })
    }

    /// Resolve the tip rule to the commit the base ruleset resolves against. A
    /// working-tree or index tip sits on the current commit, or the empty tree
    /// when HEAD is unborn.
    async fn resolve_tip(&self) -> Result<RevisionId> {
        match &self.tip {
            TipRule::WorkingCopy | TipRule::Index => {
                match self.repo.resolve_ref("HEAD").await? {
                    Some(head) => Ok(head),
                    None => self.repo.empty().await?.ok_or_else(|| {
                        Error::Source("git could not name the empty tree".to_string())
                    }),
                }
            }
            TipRule::Ref { name } => self.repo.resolve_ref(name).await?.ok_or_else(|| {
                Error::Source(format!("revision '{name}' did not resolve to a commit"))
            }),
            TipRule::Pinned { revision } => self
                .repo
                .resolve_commit(revision.as_str())
                .await?
                .ok_or_else(|| {
                    Error::Source(format!("revision '{revision}' did not resolve to a commit"))
                }),
            TipRule::ChangeId { id } => Err(Error::Source(format!(
                "a change-id tip ({id}) is not supported under git"
            ))),
        }
    }

    /// The uncommitted working copy diffed against `base`. To show untracked
    /// files as additions without disturbing the real index, we seed a throwaway
    /// index and record only the untracked, non-ignored files there as
    /// intent-to-add; modifications and deletions still show because they are
    /// never staged into the throwaway.
    async fn capture_working_copy(&self, base: &RevisionId) -> Result<String> {
        let index = self.repo.seed_temp_index().await?;
        // git add --intent-to-add writes an empty blob into the object database,
        // which fails when .git is mounted read-only. Redirect object writes to a
        // throwaway directory, keeping the repo's real objects readable as an
        // alternate, so an untracked file still shows without touching the repo.
        let scratch = tempfile::tempdir().map_err(|source| {
            Error::Source(format!(
                "could not create a temporary object directory: {source}"
            ))
        })?;
        let real_objects = self.repo.real_objects_path().await?;
        let env = GitEnv {
            index: Some(index.path()),
            scratch_objects: Some(scratch.path()),
            real_objects: Some(&real_objects),
        };
        self.repo.add_untracked_files_to_temp_index(env).await?;
        backdate_stat_cache(index.path())?;
        self.repo.diff(&[base.as_str().into()], env).await
    }
}

/// Backdate the index file at `path` so git re-reads the working tree instead of
/// trusting the stat cache copied into it. The temp index is a copy of the real
/// one, but its file is written after the working files it describes; because it
/// is newer than every entry, git's racy-git safeguard stays off and git trusts
/// the copied stat cache, missing a same-size edit whose modification time
/// coincides with the committed entry's. Backdating the index file to before
/// every entry makes them all racily clean, which forces the content comparison.
/// The chosen time is one second past the epoch rather than the epoch itself
/// because git reads a zero index timestamp as "unknown" and disables the
/// racy-git check.
///
/// The file is opened afresh by path rather than through the handle that seeded
/// it: recording untracked files rewrites the index through a temp-and-rename,
/// leaving that handle pointing at the replaced file, not the one git will read.
fn backdate_stat_cache(path: &Path) -> Result<()> {
    let pre_historic = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| Error::Source(format!("could not open the temporary index: {source}")))?;
    file.set_modified(pre_historic).map_err(|source| {
        Error::Source(format!(
            "could not set the temporary index modification time: {source}"
        ))
    })
}

#[async_trait]
impl DiffSource for GitSource {
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
            TipRule::WorkingCopy => (self.capture_working_copy(&base).await?, None),
            TipRule::Index => (
                self.repo
                    .diff(
                        &["--cached".into(), base.as_str().into()],
                        GitEnv::default(),
                    )
                    .await?,
                None,
            ),
            // Any committed tip resolves to a commit and is diffed against the
            // base; resolve_tip has already rejected a change-id tip under git.
            _ => (
                self.repo
                    .diff(
                        &[base.as_str().into(), tip.as_str().into()],
                        GitEnv::default(),
                    )
                    .await?,
                Some(tip.clone()),
            ),
        };
        let branch_hint = match &self.tip {
            TipRule::WorkingCopy | TipRule::Index => match self.repo.head_branch().await? {
                HeadBranch::On(name) => Some(name),
                HeadBranch::Detached | HeadBranch::Unknown => None,
            },
            _ => None,
        };
        Ok(CapturedDiff {
            text,
            source: SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
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
    use std::path::Path;
    use std::process::{Command, Output};

    use super::{GitRepo, GitSource, rewrite_clone_url};
    use crate::base_resolve::{ResolvedBase, RevisionResolver, resolve_base};
    use crate::base_ruleset::{BaseRuleset, parse_ruleset};
    use crate::identity::ScmType;
    use crate::record::{RevisionId, ScmSource, SourceKind, TipRule};
    use crate::session_id::SessionId;
    use crate::source::{DiffSource, FetchSource, HeadBranch, Remote, ScmRepo, TrackingBranch};

    /// Run `git` with `args` in `repo` under a laundered environment so neither
    /// the setup nor the capture under test can pick up host or per-user git
    /// configuration: the environment is emptied, `HOME` points at an empty
    /// `home` directory, and system config is disabled outright.
    fn run_git(repo: &Path, home: &Path, args: &[&str]) -> Output {
        let output = Command::new("git")
            .env_clear()
            .env("HOME", home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(["-c", "user.name=wez", "-c", "user.email=wez@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Run `git` in `repo` for its effect, asserting it succeeded.
    fn git(repo: &Path, home: &Path, args: &[&str]) {
        run_git(repo, home, args);
    }

    /// Run `git` in `repo` and return its trimmed stdout, for reading back the
    /// commit ids a test's setup produced.
    fn git_out(repo: &Path, home: &Path, args: &[&str]) -> String {
        String::from_utf8(run_git(repo, home, args).stdout)
            .expect("utf-8")
            .trim()
            .to_string()
    }

    /// Blank the variable `index <old>..<new>` blob hashes so the captured patch
    /// can be asserted whole.
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

    #[tokio::test]
    async fn a_revision_source_captures_the_commit_patch() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "add f"]);

        let head = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));
        let empty_tree = RevisionId(git_out(
            repo.path(),
            home.path(),
            &["hash-object", "-t", "tree", "/dev/null"],
        ));

        // A ref tip against a parent(@) base: for a root commit the parent falls
        // back to the empty tree, so the diff is the whole commit, as git show
        // gave before.
        let captured = GitSource::revision(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            TipRule::Ref {
                name: "HEAD".to_string(),
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
        wince::assert_eq!(stable(&captured.text), expected.to_string());
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Ref {
                    name: "HEAD".to_string()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.base_revision, Some(empty_tree));
        // The parent(@) base follows the tip, so a refresh treats its move as
        // expected.
        wince::assert_eq!(captured.base_tip_relative, true);
        wince::assert_eq!(captured.head_revision, Some(head));
    }

    #[tokio::test]
    async fn a_branch_change_tracks_its_newest_commit_across_a_recapture() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));

        // A short branch name is recorded under its full ref name, so a later
        // re-resolution never collides with a like-named tag.
        let source = GitSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            "main".to_string(),
        )
        .await
        .expect("change by branch");
        let captured = source.capture().await.expect("capture branch");
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Ref {
                    name: "refs/heads/main".to_string()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.head_revision, Some(first.clone()));

        // Advancing the branch and re-capturing the same source follows the tip
        // to the new commit, proving a Ref re-resolves rather than holding.
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write");
        git(repo.path(), home.path(), &["commit", "-qa", "-m", "second"]);
        let second = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));
        let recaptured = source.capture().await.expect("recapture branch");
        wince::assert_eq!(recaptured.head_revision, Some(second));
    }

    #[tokio::test]
    async fn a_bare_revision_change_holds_its_commit_across_a_recapture() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));

        // The same commit named as a bare revision is held, not tracked.
        let source = GitSource::change(repo.path(), BaseRuleset::new("parent(@)"), first.0.clone())
            .await
            .expect("change by revision");
        let captured = source.capture().await.expect("capture revision");
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Pinned {
                    revision: first.clone()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.head_revision, Some(first.clone()));

        // Advancing the branch leaves the pinned tip on the commit it named.
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write");
        git(repo.path(), home.path(), &["commit", "-qa", "-m", "second"]);
        let recaptured = source.capture().await.expect("recapture revision");
        wince::assert_eq!(recaptured.head_revision, Some(first));
    }

    #[tokio::test]
    async fn a_change_of_head_while_detached_is_pinned_not_tracked() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));

        // A detached HEAD is not a ref: git rev-parse --symbolic-full-name HEAD
        // echoes the literal "HEAD", which must be held as a pinned commit rather
        // than followed like a branch.
        git(
            repo.path(),
            home.path(),
            &["checkout", "-q", first.as_str()],
        );
        let source = GitSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            "HEAD".to_string(),
        )
        .await
        .expect("change by detached HEAD");
        let captured = source.capture().await.expect("capture detached HEAD");
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Pinned {
                    revision: first.clone()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.head_revision, Some(first));
    }

    #[tokio::test]
    async fn a_change_naming_no_commit_is_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "first"]);

        let error = GitSource::change(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            "nope".to_string(),
        )
        .await
        .expect_err("unresolved change");
        wince::assert_eq!(
            error.to_string(),
            "could not capture diff: 'nope' did not resolve to a commit".to_string()
        );
    }

    #[tokio::test]
    async fn a_merge_revision_shows_its_first_parent_net_change() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("main.txt"), "base\n").expect("write");
        git(repo.path(), home.path(), &["add", "main.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "base"]);
        let mainline = git_out(
            repo.path(),
            home.path(),
            &["rev-parse", "--abbrev-ref", "HEAD"],
        );

        // A feature branch adds its own file, while the mainline advances
        // independently, so the merge has genuinely divergent parents.
        git(repo.path(), home.path(), &["switch", "-q", "-c", "feature"]);
        std::fs::write(repo.path().join("feature.txt"), "feat\n").expect("write");
        git(repo.path(), home.path(), &["add", "feature.txt"]);
        git(
            repo.path(),
            home.path(),
            &["commit", "-q", "-m", "add feature"],
        );
        git(repo.path(), home.path(), &["switch", "-q", &mainline]);
        std::fs::write(repo.path().join("main2.txt"), "mainline\n").expect("write");
        git(repo.path(), home.path(), &["add", "main2.txt"]);
        git(
            repo.path(),
            home.path(),
            &["commit", "-q", "-m", "advance mainline"],
        );
        git(
            repo.path(),
            home.path(),
            &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
        );

        let head = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));
        let first_parent = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD^1"]));

        let captured = GitSource::revision(
            repo.path(),
            BaseRuleset::new("parent(@)"),
            TipRule::Ref {
                name: "HEAD".to_string(),
            },
        )
        .capture()
        .await
        .expect("capture");
        // The net change the merge brought onto the mainline is the feature
        // branch's file; git show's combined diff of this clean merge is empty.
        let expected = "\
diff --git a/feature.txt b/feature.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/feature.txt
@@ -0,0 +1 @@
+feat";
        wince::assert_eq!(stable(&captured.text), expected.to_string());
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base: BaseRuleset::new("parent(@)"),
                tip: TipRule::Ref {
                    name: "HEAD".to_string()
                },
                branch_hint: None,
            })
        );
        wince::assert_eq!(captured.base_revision, Some(first_parent));
        wince::assert_eq!(captured.base_tip_relative, true);
        wince::assert_eq!(captured.head_revision, Some(head));
    }

    #[tokio::test]
    async fn a_pinned_tip_that_no_longer_resolves_is_reported() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write");
        git(repo.path(), home.path(), &["add", "f.txt"]);
        git(repo.path(), home.path(), &["commit", "-q", "-m", "add f"]);

        // A well-formed object name that names no commit, as a gc'd or
        // rewritten-away pin would: resolve_tip must reject it with the same
        // diagnostic a missing ref gives, not pass it down to git diff.
        let gone = RevisionId("0000000000000000000000000000000000000000".to_string());
        let result = GitSource::revision(
            repo.path(),
            BaseRuleset::empty(),
            TipRule::Pinned { revision: gone },
        )
        .capture()
        .await;
        wince::assert_eq!(
            result.expect_err("pin gone").to_string(),
            "could not capture diff: revision '0000000000000000000000000000000000000000' did not \
             resolve to a commit"
                .to_string()
        );
    }

    #[tokio::test]
    async fn a_working_copy_source_captures_untracked_files_with_a_read_only_git() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("tracked.txt"), "one\n").expect("write");
        git(repo.path(), home.path(), &["add", "tracked.txt"]);
        git(
            repo.path(),
            home.path(),
            &["commit", "-q", "-m", "add tracked"],
        );

        // A modification to a tracked file and a brand-new untracked file. The
        // untracked file is what forces the intent-to-add object write.
        std::fs::write(repo.path().join("tracked.txt"), "one\ntwo\n").expect("write");
        std::fs::write(repo.path().join("fresh.txt"), "new\n").expect("write");

        // Pin the base at the current commit before the capture, matching a
        // default `wiff new`.
        let head = RevisionId(git_out(repo.path(), home.path(), &["rev-parse", "HEAD"]));
        let base = BaseRuleset::pinned(&head);

        // Strip every write bit from .git to mimic a read-only mount, capture,
        // then restore so the tempdir can be cleaned up.
        let git_dir = repo.path().join(".git");
        set_readonly_recursively(&git_dir, true);
        let result = GitSource::working_copy(repo.path(), base.clone())
            .capture()
            .await;
        set_readonly_recursively(&git_dir, false);
        let captured = result.expect("capture");

        let expected = "\
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1 @@
+new
diff --git a/tracked.txt b/tracked.txt
index HASHES
--- a/tracked.txt
+++ b/tracked.txt
@@ -1 +1,2 @@
 one
+two";
        wince::assert_eq!(stable(&captured.text), expected.to_string());
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base,
                tip: TipRule::WorkingCopy,
                // The working copy sits on main, recorded as the discovery hint.
                branch_hint: Some("refs/heads/main".to_string()),
            })
        );
        wince::assert_eq!(captured.base_revision, Some(head));
        // The base is pinned at the commit HEAD was on, not anchored to the
        // tip, so a later move would be reported.
        wince::assert_eq!(captured.base_tip_relative, false);
        wince::assert_eq!(captured.head_revision, None);
    }

    #[tokio::test]
    async fn a_working_copy_source_captures_a_repository_with_no_commits() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        // A freshly initialized repository has no commits and no index file yet.
        git(repo.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("fresh.txt"), "new\n").expect("write");

        // `wiff new` pins the base at HEAD, which falls back to the empty tree
        // when HEAD is unborn.
        let base = GitSource::pinned_base_at_head(repo.path())
            .await
            .expect("base");
        let empty_tree = RevisionId(git_out(
            repo.path(),
            home.path(),
            &["hash-object", "-t", "tree", "/dev/null"],
        ));
        let captured = GitSource::working_copy(repo.path(), base.clone())
            .capture()
            .await
            .expect("capture");

        let expected = "\
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1 @@
+new";
        wince::assert_eq!(stable(&captured.text), expected.to_string());
        wince::assert_eq!(
            captured.source,
            SourceKind::Scm(ScmSource {
                scm: ScmType::Git,
                base,
                tip: TipRule::WorkingCopy,
                // HEAD sits on the unborn `main`, recorded like any other
                // branch the review was taken on.
                branch_hint: Some("refs/heads/main".to_string()),
            })
        );
        wince::assert_eq!(captured.base_revision, Some(empty_tree));
        wince::assert_eq!(captured.base_tip_relative, false);
        wince::assert_eq!(captured.head_revision, None);
    }

    #[test]
    fn head_branch_names_an_unborn_branch_a_detached_head_and_a_non_repository() {
        let home = tempfile::tempdir().expect("home");

        // A freshly initialized repository has no commits, so HEAD points at a
        // branch that does not exist yet; it is still on that branch.
        let unborn = tempfile::tempdir().expect("unborn");
        git(unborn.path(), home.path(), &["init", "-q", "-b", "main"]);
        wince::assert_eq!(
            super::head_branch(unborn.path()),
            HeadBranch::On("refs/heads/main".to_string())
        );

        // A commit followed by a detach leaves HEAD on no branch.
        let detached = tempfile::tempdir().expect("detached");
        git(detached.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(detached.path().join("f.txt"), "a\n").expect("write");
        git(detached.path(), home.path(), &["add", "f.txt"]);
        git(detached.path(), home.path(), &["commit", "-q", "-m", "f"]);
        git(
            detached.path(),
            home.path(),
            &["checkout", "-q", "--detach"],
        );
        wince::assert_eq!(super::head_branch(detached.path()), HeadBranch::Detached);

        // A directory that is not a git repository names no branch at all.
        let bare = tempfile::tempdir().expect("bare");
        wince::assert_eq!(super::head_branch(bare.path()), HeadBranch::Unknown);
    }

    #[tokio::test]
    async fn async_head_branch_matches_the_sync_classification() {
        let home = tempfile::tempdir().expect("home");

        let unborn = tempfile::tempdir().expect("unborn");
        git(unborn.path(), home.path(), &["init", "-q", "-b", "main"]);
        wince::assert_eq!(
            GitRepo::new(unborn.path()).head_branch().await.expect("on"),
            HeadBranch::On("refs/heads/main".to_string())
        );

        let detached = tempfile::tempdir().expect("detached");
        git(detached.path(), home.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(detached.path().join("f.txt"), "a\n").expect("write");
        git(detached.path(), home.path(), &["add", "f.txt"]);
        git(detached.path(), home.path(), &["commit", "-q", "-m", "f"]);
        git(
            detached.path(),
            home.path(),
            &["checkout", "-q", "--detach"],
        );
        wince::assert_eq!(
            GitRepo::new(detached.path())
                .head_branch()
                .await
                .expect("detached"),
            HeadBranch::Detached
        );

        let bare = tempfile::tempdir().expect("bare");
        wince::assert_eq!(
            GitRepo::new(bare.path()).head_branch().await.expect("bare"),
            HeadBranch::Unknown
        );
    }

    #[tokio::test]
    async fn backdating_reaches_the_index_left_by_recording_untracked_files() {
        use std::time::{Duration, SystemTime};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index");
        std::fs::write(&path, b"seed").expect("seed");
        // Recording untracked files rewrites the index through a temp-and-rename;
        // mimic that by renaming a fresh file over the path, orphaning the file
        // the original handle held.
        let replacement = dir.path().join("index.new");
        std::fs::write(&replacement, b"rewritten").expect("replacement");
        std::fs::rename(&replacement, &path).expect("rename");

        super::backdate_stat_cache(&path).expect("backdate");

        let mtime = std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("modified");
        wince::assert_eq!(mtime, SystemTime::UNIX_EPOCH + Duration::from_secs(1));
    }

    /// Toggle the read-only bit on every file and directory under `root`
    /// (including `root` itself), mimicking a read-only mount closely enough to
    /// reject object writes into `.git`.
    fn set_readonly_recursively(root: &Path, readonly: bool) {
        fn set(path: &Path, readonly: bool) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).expect("read_dir") {
                    set(&entry.expect("entry").path(), readonly);
                }
            }
            let mut perms = std::fs::metadata(path).expect("metadata").permissions();
            perms.set_readonly(readonly);
            std::fs::set_permissions(path, perms).expect("set_permissions");
        }
        set(root, readonly);
    }

    /// A repository with two commits on `main` and a third on a `feature`
    /// branch forked from the first, returning a repo handle and the commit ids
    /// (first, second, feature tip) for a resolver test to assert against.
    fn forked_repo(
        repo: &tempfile::TempDir,
        home: &tempfile::TempDir,
    ) -> (GitRepo, RevisionId, RevisionId, RevisionId) {
        let (r, h) = (repo.path(), home.path());
        git(r, h, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("f.txt"), "a\n").expect("write");
        git(r, h, &["add", "f.txt"]);
        git(r, h, &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        std::fs::write(r.join("f.txt"), "a\nb\n").expect("write");
        git(r, h, &["commit", "-qa", "-m", "second"]);
        let second = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        git(r, h, &["checkout", "-q", "-b", "feature", first.as_str()]);
        std::fs::write(r.join("g.txt"), "c\n").expect("write");
        git(r, h, &["add", "g.txt"]);
        git(r, h, &["commit", "-q", "-m", "feature work"]);
        let feature = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        (GitRepo::new(r), first, second, feature)
    }

    #[tokio::test]
    async fn resolving_a_ref_yields_its_commit_and_an_unknown_ref_yields_none() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, _first, second, _feature) = forked_repo(&repo, &home);
        wince::assert_eq!(
            source.resolve_ref("main").await.expect("resolve"),
            Some(second)
        );
        wince::assert_eq!(source.resolve_ref("nope").await.expect("resolve"), None);
    }

    #[tokio::test]
    async fn parent_of_a_commit_is_its_first_parent() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, first, second, _feature) = forked_repo(&repo, &home);
        wince::assert_eq!(source.parent(&second).await.expect("parent"), Some(first));
    }

    #[tokio::test]
    async fn merge_base_of_a_branch_and_the_tip_is_their_fork_point() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, first, second, feature) = forked_repo(&repo, &home);
        wince::assert_eq!(
            source
                .merge_base(&second, &feature)
                .await
                .expect("merge-base"),
            Some(first)
        );
    }

    #[tokio::test]
    async fn resolving_a_merge_base_ruleset_finds_the_fork_point() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, first, _second, feature) = forked_repo(&repo, &home);
        let ruleset = parse_ruleset("merge-base(name(main))").expect("parse");
        let base = resolve_base(&ruleset, &feature, &source)
            .await
            .expect("resolve");
        wince::assert_eq!(
            base,
            Some(ResolvedBase {
                revision: first,
                tip_relative: false,
            })
        );
    }

    #[tokio::test]
    async fn resolving_empty_yields_the_repositorys_empty_tree() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, _first, _second, feature) = forked_repo(&repo, &home);
        let expected = git_out(
            repo.path(),
            home.path(),
            &["hash-object", "-t", "tree", "/dev/null"],
        );
        let ruleset = parse_ruleset("empty").expect("parse");
        let base = resolve_base(&ruleset, &feature, &source)
            .await
            .expect("resolve");
        wince::assert_eq!(
            base,
            Some(ResolvedBase {
                revision: RevisionId(expected),
                tip_relative: false,
            })
        );
    }

    #[tokio::test]
    async fn trunk_falls_back_to_a_local_main_when_there_is_no_remote() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        // No remote is configured, so origin/HEAD is absent and trunk resolves
        // through the local main fallback to its commit.
        let (source, _first, second, _feature) = forked_repo(&repo, &home);
        wince::assert_eq!(source.trunk().await.expect("trunk"), Some(second));
    }

    #[tokio::test]
    async fn upstream_resolves_the_tracking_branch() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        // HEAD is on feature; point its upstream at main so @{upstream} resolves
        // to main's commit.
        let (source, _first, second, _feature) = forked_repo(&repo, &home);
        git(
            repo.path(),
            home.path(),
            &["branch", "--set-upstream-to=main", "feature"],
        );
        wince::assert_eq!(source.upstream().await.expect("upstream"), Some(second));
    }

    #[tokio::test]
    async fn a_ref_name_that_looks_like_an_option_resolves_to_none_not_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        // --end-of-options keeps a dash-leading ref name from being read as a
        // git option: it is looked up as a (nonexistent) ref and falls through
        // to None rather than erroring on an unknown flag.
        let (source, _first, _second, _feature) = forked_repo(&repo, &home);
        wince::assert_eq!(source.resolve_ref("--all").await.expect("resolve"), None);
    }

    #[tokio::test]
    async fn parent_of_a_root_commit_falls_back_to_the_empty_tree() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (r, h) = (repo.path(), home.path());
        git(r, h, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("f.txt"), "a\n").expect("write");
        git(r, h, &["add", "f.txt"]);
        git(r, h, &["commit", "-q", "-m", "root"]);
        let root = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        let empty = git_out(r, h, &["hash-object", "-t", "tree", "/dev/null"]);
        let source = GitRepo::new(r);
        wince::assert_eq!(
            source.parent(&root).await.expect("parent"),
            Some(RevisionId(empty))
        );
    }

    #[tokio::test]
    async fn merge_base_with_a_nonexistent_revision_is_an_error_not_a_fall_through() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (source, _first, _second, feature) = forked_repo(&repo, &home);
        let bogus = RevisionId("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string());
        let error = source
            .merge_base(&bogus, &feature)
            .await
            .expect_err("merge-base fails");
        // git's exit 128 for a bad object is a genuine failure, reported rather
        // than swallowed as a fall-through. The trailing stderr detail varies by
        // git version, so only the stable head of the message is asserted.
        let message = error.to_string();
        let normalized = match message.split_once("): ") {
            Some((head, _)) => format!("{head}): STDERR"),
            None => message,
        };
        wince::assert_eq!(
            normalized,
            "git merge-base failed (exit status: 128): STDERR".to_string()
        );
    }

    /// A session id distinct across tests so pin refs never collide.
    fn session(n: u64) -> SessionId {
        format!("{n:09}").parse().expect("a valid session id")
    }

    #[tokio::test]
    async fn remotes_lists_each_configured_remote_with_its_fetch_url() {
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (w, h) = (work.path(), home.path());
        git(w, h, &["init", "-q", "-b", "main"]);
        git(
            w,
            h,
            &["remote", "add", "origin", "git@github.com:octo/demo.git"],
        );
        git(
            w,
            h,
            &["remote", "add", "fork", "https://codeberg.org/me/demo.git"],
        );

        let repo = GitRepo::new(w);
        let remotes = repo.remotes().await.expect("remotes");
        wince::assert_eq!(
            remotes,
            vec![
                Remote {
                    name: "origin".to_string(),
                    url: "git@github.com:octo/demo.git".to_string(),
                },
                Remote {
                    name: "fork".to_string(),
                    url: "https://codeberg.org/me/demo.git".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn remotes_reports_a_multi_url_remote_once_by_its_fetch_url() {
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (w, h) = (work.path(), home.path());
        git(w, h, &["init", "-q", "-b", "main"]);
        git(
            w,
            h,
            &["remote", "add", "origin", "git@github.com:octo/demo.git"],
        );
        // A second URL, as `git remote set-url --add` writes it; git fetches
        // from the first, so the added one must not produce a second entry.
        git(
            w,
            h,
            &[
                "remote",
                "set-url",
                "--add",
                "origin",
                "https://github.com/octo/demo.git",
            ],
        );

        let repo = GitRepo::new(w);
        let remotes = repo.remotes().await.expect("remotes");
        wince::assert_eq!(
            remotes,
            vec![Remote {
                name: "origin".to_string(),
                url: "git@github.com:octo/demo.git".to_string(),
            }]
        );
    }

    /// A set of `url.<base>.insteadOf` rewrites in config order, including two
    /// aliases sharing a base, one alias extending another, and a repeated alias.
    fn alias_rewrites() -> Vec<(String, String)> {
        [
            ("octo:", "git@github.com:octo/"),
            ("oc:", "git@github.com:octo/"),
            ("octo:sub/", "ssh://longer.example/"),
            ("gh:", "https://github.com/"),
            ("gh:", "https://second.example/"),
        ]
        .into_iter()
        .map(|(alias, base)| (alias.to_string(), base.to_string()))
        .collect()
    }

    #[test]
    fn rewrite_clone_url_expands_the_longest_matching_alias() {
        let mapped: Vec<String> = [
            "octo:demo.git",
            "octo:sub/demo.git",
            "oc:demo.git",
            "gh:octo/demo.git",
            "git@github.com:octo/demo.git",
            "octo",
        ]
        .into_iter()
        .map(|clone_url| {
            format!(
                "{clone_url} -> {}",
                rewrite_clone_url(clone_url, &alias_rewrites())
            )
        })
        .collect();
        wince::assert_eq!(
            mapped.join("\n"),
            "octo:demo.git -> git@github.com:octo/demo.git\n\
             octo:sub/demo.git -> ssh://longer.example/demo.git\n\
             oc:demo.git -> git@github.com:octo/demo.git\n\
             gh:octo/demo.git -> https://github.com/octo/demo.git\n\
             git@github.com:octo/demo.git -> git@github.com:octo/demo.git\n\
             octo -> octo"
        );
    }

    #[test]
    fn rewrite_clone_url_leaves_a_url_alone_when_nothing_is_configured() {
        wince::assert_eq!(
            rewrite_clone_url("git@github.com:octo/demo.git", &[]),
            "git@github.com:octo/demo.git".to_string()
        );
    }

    #[tokio::test]
    async fn remotes_expands_an_alias_a_url_rewrite_configures() {
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (w, h) = (work.path(), home.path());
        git(w, h, &["init", "-q", "-b", "main"]);
        // The alias a `url.<base>.insteadOf` names is what a remote is
        // configured with; on its own it names neither host nor repository, so
        // it must come back expanded the way git would contact it.
        git(
            w,
            h,
            &["config", "url.git@github.com:octo/.insteadOf", "octo:"],
        );
        // An empty alias prefix-matches every URL; git ignores it, and so must
        // the remote below.
        git(
            w,
            h,
            &["config", "url.ssh://ignored.example/.insteadOf", ""],
        );
        git(w, h, &["remote", "add", "origin", "octo:demo.git"]);
        git(
            w,
            h,
            &["remote", "add", "fork", "https://codeberg.org/me/demo.git"],
        );

        let repo = GitRepo::new(w);
        let remotes = repo.remotes().await.expect("remotes");
        wince::assert_eq!(
            remotes,
            vec![
                Remote {
                    name: "origin".to_string(),
                    url: "git@github.com:octo/demo.git".to_string(),
                },
                Remote {
                    name: "fork".to_string(),
                    url: "https://codeberg.org/me/demo.git".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn remotes_is_empty_for_a_repo_with_no_remotes() {
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(work.path(), home.path(), &["init", "-q", "-b", "main"]);

        let repo = GitRepo::new(work.path());
        let remotes = repo.remotes().await.expect("remotes");
        wince::assert_eq!(remotes, Vec::<Remote>::new());
    }

    #[tokio::test]
    async fn fetch_pinned_brings_a_ref_down_and_pins_it_under_the_session() {
        // A separate origin repo holds the commit to fetch; the working repo
        // fetches it by path and ref, as a forge fetch would by URL and ref.
        let origin = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (o, w, h) = (origin.path(), work.path(), home.path());
        git(o, h, &["init", "-q", "-b", "main"]);
        std::fs::write(o.join("f.txt"), "a\n").expect("write");
        git(o, h, &["add", "f.txt"]);
        git(o, h, &["commit", "-q", "-m", "origin work"]);
        let commit = RevisionId(git_out(o, h, &["rev-parse", "HEAD"]));
        git(w, h, &["init", "-q", "-b", "main"]);

        let repo = GitRepo::new(w);
        let sess = session(1);
        let source = FetchSource::Git {
            url: o.to_string_lossy().into_owned(),
            git_ref: "refs/heads/main".to_string(),
            commit: commit.clone(),
        };
        let resolved = repo.fetch_pinned(&source, sess).await.expect("fetch");
        wince::assert_eq!(resolved, commit.clone());
        // The pin resolves to the fetched commit, held under refs/wiff and not
        // as a branch anyone would see.
        let pinned = git_out(w, h, &["rev-parse", &format!("refs/wiff/{sess}/head")]);
        wince::assert_eq!(pinned, commit.to_string());
        let branches = git_out(w, h, &["branch", "--list", "--format=%(refname)"]);
        wince::assert_eq!(branches, String::new());
    }

    #[tokio::test]
    async fn fetch_pinned_rejects_a_ref_that_resolves_to_an_unexpected_commit() {
        let origin = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (o, w, h) = (origin.path(), work.path(), home.path());
        git(o, h, &["init", "-q", "-b", "main"]);
        std::fs::write(o.join("f.txt"), "a\n").expect("write");
        git(o, h, &["add", "f.txt"]);
        git(o, h, &["commit", "-q", "-m", "origin work"]);
        git(w, h, &["init", "-q", "-b", "main"]);

        let repo = GitRepo::new(w);
        let stale = RevisionId("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string());
        let source = FetchSource::Git {
            url: o.to_string_lossy().into_owned(),
            git_ref: "refs/heads/main".to_string(),
            commit: stale,
        };
        let error = repo
            .fetch_pinned(&source, session(2))
            .await
            .expect_err("fetch rejects a moved ref");
        let head = git_out(o, h, &["rev-parse", "HEAD"]);
        // The rejected fetch leaves no pin resolvable under the session.
        let refs = git_out(w, h, &["for-each-ref", "--format=%(refname)", "refs/wiff/"]);
        wince::assert_eq!(
            (error.to_string(), refs),
            (
                format!(
                    "fetched refs/heads/main resolved to {head}, \
                     but the forge reported deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                ),
                String::new()
            )
        );
    }

    #[tokio::test]
    async fn fetch_pinned_re_fetch_that_mismatches_keeps_a_prior_good_pin() {
        let origin = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (o, w, h) = (origin.path(), work.path(), home.path());
        git(o, h, &["init", "-q", "-b", "main"]);
        std::fs::write(o.join("f.txt"), "a\n").expect("write");
        git(o, h, &["add", "f.txt"]);
        git(o, h, &["commit", "-q", "-m", "origin work"]);
        let commit = RevisionId(git_out(o, h, &["rev-parse", "HEAD"]));
        git(w, h, &["init", "-q", "-b", "main"]);

        let repo = GitRepo::new(w);
        let sess = session(4);
        // A first, good fetch pins the head at the real commit.
        let good = FetchSource::Git {
            url: o.to_string_lossy().into_owned(),
            git_ref: "refs/heads/main".to_string(),
            commit: commit.clone(),
        };
        repo.fetch_pinned(&good, sess).await.expect("first fetch");

        // A second fetch of the same ref, but the forge now promises a commit
        // that does not match, is rejected.
        let stale = FetchSource::Git {
            url: o.to_string_lossy().into_owned(),
            git_ref: "refs/heads/main".to_string(),
            commit: RevisionId("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string()),
        };
        let error = repo
            .fetch_pinned(&stale, sess)
            .await
            .expect_err("mismatched re-fetch is rejected");

        // The head pin still resolves to the first good commit, and no scratch
        // ref was left behind.
        let pinned = git_out(w, h, &["rev-parse", &format!("refs/wiff/{sess}/head")]);
        let refs = git_out(w, h, &["for-each-ref", "--format=%(refname)", "refs/wiff/"]);
        wince::assert_eq!(
            (error.to_string(), pinned, refs),
            (
                format!(
                    "fetched refs/heads/main resolved to {commit}, \
                     but the forge reported deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                ),
                commit.to_string(),
                format!("refs/wiff/{sess}/head")
            )
        );
    }

    #[tokio::test]
    async fn fetch_base_pins_a_reported_tip_the_branch_has_since_moved_past() {
        // The base branch lives in a separate origin repo, as the pull request's
        // target branch lives on its forge. The forge reports the tip it saw at
        // its last sync, but the branch has advanced two commits past it; the
        // fetch must still bring that older commit down and pin it.
        let origin = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (o, w, h) = (origin.path(), work.path(), home.path());
        git(o, h, &["init", "-q", "-b", "main"]);
        std::fs::write(o.join("f.txt"), "a\n").expect("write");
        git(o, h, &["add", "f.txt"]);
        git(o, h, &["commit", "-q", "-m", "reported tip"]);
        let reported = RevisionId(git_out(o, h, &["rev-parse", "HEAD"]));
        std::fs::write(o.join("f.txt"), "a\nb\n").expect("write");
        git(o, h, &["commit", "-qa", "-m", "advance one"]);
        std::fs::write(o.join("f.txt"), "a\nb\nc\n").expect("write");
        git(o, h, &["commit", "-qa", "-m", "advance two"]);
        git(w, h, &["init", "-q", "-b", "main"]);

        let repo = GitRepo::new(w);
        let sess = session(3);
        let source = FetchSource::Git {
            url: o.to_string_lossy().into_owned(),
            git_ref: "refs/heads/main".to_string(),
            commit: reported.clone(),
        };
        let resolved = repo.fetch_base(&source, sess).await.expect("fetch base");
        wince::assert_eq!(resolved, reported.clone());
        // The base pin resolves to the reported commit, not the branch tip, and
        // no head pin or scratch ref was created alongside it.
        let pinned = git_out(w, h, &["rev-parse", &format!("refs/wiff/{sess}/base")]);
        let refs = git_out(w, h, &["for-each-ref", "--format=%(refname)", "refs/wiff/"]);
        wince::assert_eq!(
            (pinned, refs),
            (reported.to_string(), format!("refs/wiff/{sess}/base"))
        );
    }

    #[tokio::test]
    async fn fetch_base_rejects_a_reported_commit_absent_from_the_branch() {
        // The forge reports a base commit that is not on the fetched branch, as
        // a rebase or force-push of the target would leave behind; the fetch
        // must reject it rather than pin a commit off the branch.
        let origin = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (o, w, h) = (origin.path(), work.path(), home.path());
        git(o, h, &["init", "-q", "-b", "main"]);
        std::fs::write(o.join("f.txt"), "a\n").expect("write");
        git(o, h, &["add", "f.txt"]);
        git(o, h, &["commit", "-q", "-m", "origin work"]);
        git(w, h, &["init", "-q", "-b", "main"]);

        let repo = GitRepo::new(w);
        let sess = session(3);
        let absent = RevisionId("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string());
        let source = FetchSource::Git {
            url: o.to_string_lossy().into_owned(),
            git_ref: "refs/heads/main".to_string(),
            commit: absent.clone(),
        };
        let error = repo
            .fetch_base(&source, sess)
            .await
            .expect_err("an absent base commit is rejected");
        // The rejected fetch leaves no pin resolvable under the session.
        let refs = git_out(w, h, &["for-each-ref", "--format=%(refname)", "refs/wiff/"]);
        wince::assert_eq!(
            (error.to_string(), refs),
            (
                format!(
                    "the pull request's base {absent} is not on refs/heads/main fetched from {}; \
                     the target branch may have been rewritten since the pull request was last \
                     synced",
                    o.to_string_lossy()
                ),
                String::new()
            )
        );
    }

    #[tokio::test]
    async fn remove_pins_deletes_both_pins_and_tolerates_their_absence() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (r, h) = (repo_dir.path(), home.path());
        git(r, h, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("f.txt"), "a\n").expect("write");
        git(r, h, &["add", "f.txt"]);
        git(r, h, &["commit", "-q", "-m", "c"]);
        let commit = RevisionId(git_out(r, h, &["rev-parse", "HEAD"]));
        let repo = GitRepo::new(r);
        let sess = session(4);

        // Removing with no pins present is a no-op, not an error.
        repo.remove_pins(sess).await.expect("remove absent pins");

        repo.update_ref(&super::pin_ref(sess, super::PinSlot::Base), &commit)
            .await
            .expect("pin base");
        repo.update_ref(&super::pin_ref(sess, super::PinSlot::Head), &commit)
            .await
            .expect("pin head");
        // An `incoming-head` scratch ref, as an interrupted fetch would leave
        // behind; remove_pins must clear the whole namespace, not just the pins.
        repo.update_ref(&format!("refs/wiff/{sess}/incoming-head"), &commit)
            .await
            .expect("plant scratch ref");
        repo.remove_pins(sess).await.expect("remove pins");

        let refs = git_out(r, h, &["for-each-ref", "--format=%(refname)", "refs/wiff/"]);
        wince::assert_eq!(refs, String::new());
    }

    #[tokio::test]
    async fn publish_branch_pushes_to_the_remote_and_records_tracking() {
        let bare = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (b, w, h) = (bare.path(), work.path(), home.path());
        git(b, h, &["init", "-q", "--bare"]);
        git(w, h, &["init", "-q", "-b", "main"]);
        git(w, h, &["remote", "add", "origin", &b.to_string_lossy()]);
        std::fs::write(w.join("f.txt"), "a\n").expect("write");
        git(w, h, &["add", "f.txt"]);
        git(w, h, &["commit", "-q", "-m", "work"]);
        let commit = RevisionId(git_out(w, h, &["rev-parse", "HEAD"]));

        let repo = GitRepo::new(w);
        repo.publish_branch("origin", "my-feature", &commit)
            .await
            .expect("publish");

        // The remote holds the published branch at the reviewed commit.
        let remote_tip = git_out(b, h, &["rev-parse", "refs/heads/my-feature"]);
        wince::assert_eq!(remote_tip, commit.to_string());
        // The local branch now tracks it, so a plain `git push` follows.
        let remote = git_out(w, h, &["config", "branch.main.remote"]);
        let merge = git_out(w, h, &["config", "branch.main.merge"]);
        wince::assert_eq!(
            (remote, merge),
            ("origin".to_string(), "refs/heads/my-feature".to_string())
        );
    }

    #[tokio::test]
    async fn publish_branch_refuses_a_commit_that_is_not_the_current_branch_tip() {
        let bare = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (b, w, h) = (bare.path(), work.path(), home.path());
        git(b, h, &["init", "-q", "--bare"]);
        git(w, h, &["init", "-q", "-b", "main"]);
        git(w, h, &["remote", "add", "origin", &b.to_string_lossy()]);
        std::fs::write(w.join("f.txt"), "a\n").expect("write");
        git(w, h, &["add", "f.txt"]);
        git(w, h, &["commit", "-q", "-m", "first"]);
        let first = RevisionId(git_out(w, h, &["rev-parse", "HEAD"]));
        // Move the branch past `first`, so publishing `first` no longer matches
        // the branch tracking would be pointed at.
        std::fs::write(w.join("f.txt"), "a\nb\n").expect("write");
        git(w, h, &["commit", "-qa", "-m", "second"]);

        let repo = GitRepo::new(w);
        let error = repo
            .publish_branch("origin", "my-feature", &first)
            .await
            .expect_err("stale commit is refused");
        // The guard fires before any push, so the remote gained no branch.
        let remote_branches = git_out(
            b,
            h,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"],
        );
        wince::assert_eq!(
            (error.to_string(), remote_branches),
            (
                format!("cannot publish {first}: it is not the tip of main"),
                String::new(),
            )
        );
    }

    #[tokio::test]
    async fn publish_branch_leaves_a_preexisting_upstream_untouched() {
        let bare = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (b, w, h) = (bare.path(), work.path(), home.path());
        git(b, h, &["init", "-q", "--bare"]);
        git(w, h, &["init", "-q", "-b", "main"]);
        git(w, h, &["remote", "add", "origin", &b.to_string_lossy()]);
        std::fs::write(w.join("f.txt"), "a\n").expect("write");
        git(w, h, &["add", "f.txt"]);
        git(w, h, &["commit", "-q", "-m", "work"]);
        let commit = RevisionId(git_out(w, h, &["rev-parse", "HEAD"]));
        // A tracking config the user already set for their branch.
        git(w, h, &["config", "branch.main.remote", "origin"]);
        git(w, h, &["config", "branch.main.merge", "refs/heads/main"]);

        let repo = GitRepo::new(w);
        repo.publish_branch("origin", "my-feature", &commit)
            .await
            .expect("publish");

        // The push still happened, but the existing upstream was left as it was.
        let remote_tip = git_out(b, h, &["rev-parse", "refs/heads/my-feature"]);
        let merge = git_out(w, h, &["config", "branch.main.merge"]);
        wince::assert_eq!(
            (remote_tip, merge),
            (commit.to_string(), "refs/heads/main".to_string())
        );
    }

    #[tokio::test]
    async fn publish_branch_completes_a_half_configured_upstream() {
        let bare = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (b, w, h) = (bare.path(), work.path(), home.path());
        git(b, h, &["init", "-q", "--bare"]);
        git(w, h, &["init", "-q", "-b", "main"]);
        git(w, h, &["remote", "add", "origin", &b.to_string_lossy()]);
        std::fs::write(w.join("f.txt"), "a\n").expect("write");
        git(w, h, &["add", "f.txt"]);
        git(w, h, &["commit", "-q", "-m", "work"]);
        let commit = RevisionId(git_out(w, h, &["rev-parse", "HEAD"]));
        // Only `.merge` is set, with no `.remote`: a half-configured state that
        // a plain `git push` cannot act on, so publishing completes it rather
        // than treating the branch as already tracking.
        git(
            w,
            h,
            &["config", "branch.main.merge", "refs/heads/leftover"],
        );

        let repo = GitRepo::new(w);
        repo.publish_branch("origin", "my-feature", &commit)
            .await
            .expect("publish");

        // Both halves now name the published branch, a working upstream.
        let remote = git_out(w, h, &["config", "branch.main.remote"]);
        let merge = git_out(w, h, &["config", "branch.main.merge"]);
        wince::assert_eq!(
            (remote, merge),
            ("origin".to_string(), "refs/heads/my-feature".to_string())
        );
    }

    #[tokio::test]
    async fn working_tree_is_clean_tracks_staged_unstaged_and_untracked() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (r, h) = (repo_dir.path(), home.path());
        git(r, h, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("f.txt"), "a\n").expect("write");
        std::fs::write(r.join(".gitignore"), "ignored.txt\n").expect("write");
        git(r, h, &["add", "f.txt", ".gitignore"]);
        git(r, h, &["commit", "-q", "-m", "c"]);
        let repo = GitRepo::new(r);

        let committed = repo.working_tree_is_clean().await.expect("clean check");

        // An ignored file leaves the tree clean: a working copy review excludes it,
        // so it is not part of what publishing would send.
        std::fs::write(r.join("ignored.txt"), "junk\n").expect("write");
        let with_ignored = repo.working_tree_is_clean().await.expect("clean check");

        // A non-ignored untracked file makes it dirty: a working copy review would
        // capture it as a new-file addition absent from HEAD.
        std::fs::write(r.join("scratch.txt"), "new\n").expect("write");
        let with_untracked = repo.working_tree_is_clean().await.expect("clean check");
        std::fs::remove_file(r.join("scratch.txt")).expect("remove");

        // An unstaged edit to a tracked file makes it dirty.
        std::fs::write(r.join("f.txt"), "a\nb\n").expect("write");
        let with_unstaged = repo.working_tree_is_clean().await.expect("clean check");

        // Staging the edit keeps it dirty.
        git(r, h, &["add", "f.txt"]);
        let with_staged = repo.working_tree_is_clean().await.expect("clean check");

        wince::assert_eq!(
            (
                committed,
                with_ignored,
                with_untracked,
                with_unstaged,
                with_staged
            ),
            (true, true, false, false, false)
        );
    }

    #[tokio::test]
    async fn remote_branch_reports_a_present_branchs_commit_and_none_when_absent() {
        let bare = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (b, w, h) = (bare.path(), work.path(), home.path());
        git(b, h, &["init", "-q", "--bare"]);
        git(w, h, &["init", "-q", "-b", "main"]);
        git(w, h, &["remote", "add", "origin", &b.to_string_lossy()]);
        std::fs::write(w.join("f.txt"), "a\n").expect("write");
        git(w, h, &["add", "f.txt"]);
        git(w, h, &["commit", "-q", "-m", "work"]);
        git(w, h, &["push", "-q", "origin", "HEAD:refs/heads/published"]);
        let commit = RevisionId(git_out(w, h, &["rev-parse", "HEAD"]));
        let repo = GitRepo::new(w);

        let present = repo
            .remote_branch("origin", "published")
            .await
            .expect("query present");
        let absent = repo
            .remote_branch("origin", "never-pushed")
            .await
            .expect("query absent");
        // A name bearing glob metacharacters is matched literally: ls-remote
        // would glob "publishe?" onto "published", but the exact refname check
        // rejects it rather than reporting a different branch as this one.
        let globbed = repo
            .remote_branch("origin", "publishe?")
            .await
            .expect("query glob");

        wince::assert_eq!((present, absent, globbed), (Some(commit), None, None));
    }

    #[tokio::test]
    async fn current_upstream_reports_the_tracked_branch_and_none_when_unset() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (r, h) = (repo_dir.path(), home.path());
        git(r, h, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("f.txt"), "a\n").expect("write");
        git(r, h, &["add", "f.txt"]);
        git(r, h, &["commit", "-q", "-m", "c"]);
        let repo = GitRepo::new(r);

        let unset = repo.current_upstream().await.expect("query unset");

        // A configured upstream is reported as its remote and plain branch name.
        git(r, h, &["config", "branch.main.remote", "origin"]);
        git(r, h, &["config", "branch.main.merge", "refs/heads/trunk"]);
        let tracking = repo.current_upstream().await.expect("query set");

        // A half-configured branch (only `.merge`) is reported as no upstream.
        git(r, h, &["config", "--unset", "branch.main.remote"]);
        let half = repo.current_upstream().await.expect("query half");

        wince::assert_eq!(
            (unset, tracking, half),
            (
                None,
                Some(TrackingBranch {
                    remote: "origin".to_string(),
                    branch: "trunk".to_string(),
                }),
                None,
            )
        );
    }
}
