//! Where a session's diff comes from.
//!
//! A [`DiffSource`] produces a unified diff as text, tagged with the
//! [`SourceKind`] that records how it was obtained and whether it can be
//! regenerated later. Capture is async so a source can do IO (running a
//! subprocess now, reaching a remote forge later) without blocking. Each
//! concrete source lives in its own submodule; v0 ships [`git`]. A diff already
//! in hand is itself a source: [`CapturedDiff`] implements [`DiffSource`] by
//! yielding a clone of itself, so a diff obtained by any means (piped on stdin,
//! read from a file, fetched over RPC) integrates through the same trait.

pub mod explore;
pub mod git;
pub mod jj;

use std::path::Path;

use async_trait::async_trait;

use crate::error::Result;
use crate::identity::ScmType;
use crate::record::{RevisionId, SourceKind};
use crate::session_id::SessionId;

pub use explore::{ExploreCapture, SkipReason, capture_explore};
pub use git::{GitRepo, GitSource};
pub use jj::{JjRepo, JjSource};

/// The checked-out branch of a repository, distinguishing a real detached head
/// from a transient failure to reach the scm. A detached head is a repository
/// state a caller can act on, while an unreachable scm reports nothing about the
/// branch at all; collapsing the two would let a momentary fault masquerade as a
/// detached head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadBranch {
    /// Checked out on this branch, named by its full ref (`refs/heads/...`).
    On(String),
    /// The head is detached, on no branch.
    Detached,
    /// The scm could not be reached, or gave an answer that could not be read.
    Unknown,
}

/// Returns the branch state of the repository at `repo_root`. SCMs without a
/// branch concept (Mercurial, Sapling) report [`HeadBranch::Unknown`].
pub fn head_branch(repo_root: &Path, scm: ScmType) -> HeadBranch {
    match scm {
        ScmType::Git => git::head_branch(repo_root),
        ScmType::Jujutsu => jj::head_branch(repo_root),
        ScmType::Sapling | ScmType::Mercurial => HeadBranch::Unknown,
    }
}

/// The branch the repository at `repo_root` currently has checked out, as a full
/// ref name (`refs/heads/...`), or `None` when the head is detached, the scm has
/// no such notion, or git cannot be reached. Session discovery uses this to
/// prefer the session that reviews the current branch, where a detached head and
/// an unreachable scm are alike in offering no branch to match.
pub fn current_branch(repo_root: &Path, scm: ScmType) -> Option<String> {
    match head_branch(repo_root, scm) {
        HeadBranch::On(name) => Some(name),
        HeadBranch::Detached | HeadBranch::Unknown => None,
    }
}

/// A diff captured from a source, ready to record as a review's next version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedDiff {
    /// The unified diff text.
    pub text: String,
    /// How the diff was obtained.
    pub source: SourceKind,
    /// The base commit the diff was captured against, when the source has an
    /// authoritative base.
    pub base_revision: Option<RevisionId>,
    /// Whether the resolved base is anchored to the tip under review (e.g. via
    /// `parent(@)`), and so expected to move with it. Meaningful only when
    /// `base_revision` is set.
    pub base_tip_relative: bool,
    /// The tip commit the diff was captured at, absent for a working-tree or
    /// index capture whose tip is the uncommitted state.
    pub head_revision: Option<RevisionId>,
}

/// A producer of unified diff text.
#[async_trait]
pub trait DiffSource {
    /// Capture the current diff from this source.
    async fn capture(&self) -> Result<CapturedDiff>;
}

#[async_trait]
impl DiffSource for CapturedDiff {
    async fn capture(&self) -> Result<CapturedDiff> {
        Ok(self.clone())
    }
}

/// How to bring a pull request's commits into a local repo. The variant names
/// the wire protocol the forge's repository speaks, not the tool that runs
/// locally: a git working copy and a jj working copy both satisfy
/// [`FetchSource::Git`], the former by shelling out to git and the latter
/// through its git backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchSource {
    /// Fetch `git_ref` from `url` to bring `commit` into the local repo. The
    /// forge adapter has already chosen between the pull-ref namespace and the
    /// head repository, so this holds one resolved fetch either way.
    Git {
        /// The repository the forge adapter chose to fetch from.
        url: String,
        /// The ref within that repository to fetch.
        git_ref: String,
        /// The commit the forge reports for this fetch. How it relates to the
        /// fetched ref depends on the consumer: a head fetch requires the ref to
        /// resolve to it, while a base fetch only requires it to be reachable
        /// from the ref's tip.
        commit: RevisionId,
    },
}

impl FetchSource {
    /// The commit the forge reports for this fetch.
    pub fn commit(&self) -> &RevisionId {
        match self {
            FetchSource::Git { commit, .. } => commit,
        }
    }
}

/// A remote of the local repository: its local name paired with the clone URL
/// it points at. The URL is the one the scm would fetch from, with any of its
/// own alias rewrites (git's `url.<base>.insteadOf`) already applied; decomposing
/// it into a host, owner, and repository is the forge layer's concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The local name of the remote, such as `origin`.
    pub name: String,
    /// The clone URL the remote fetches from, as an `https://` URL or an
    /// scp-style `git@host:owner/repo.git`.
    pub url: String,
}

/// The remote branch a local branch tracks, as its remote's local name paired
/// with the branch name on that remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackingBranch {
    /// The local name of the remote, such as `origin`.
    pub remote: String,
    /// The branch name on that remote, such as `main`, without any ref prefix.
    pub branch: String,
}

/// Local repository operations forge support needs beyond producing diff text:
/// fetching a forge's commits, publishing a branch, and managing pins.
#[async_trait]
pub trait ScmRepo {
    /// Fetch `source` into the local repo, pin the fetched commit under the
    /// session's `head` pin, and return the commit it resolved to. Fails when
    /// this SCM cannot speak the protocol `source` names, or when the fetched
    /// ref does not resolve to the commit `source` expects.
    async fn fetch_pinned(&self, source: &FetchSource, session: SessionId) -> Result<RevisionId>;

    /// Fetch the target branch `source` names, pin the commit `source` reports
    /// under the session's `base` pin, and return it. Unlike
    /// [`fetch_pinned`](Self::fetch_pinned), the reported commit need not be the
    /// fetched ref's tip, only reachable from it: `source` names the pull
    /// request's target branch and the tip the forge saw at its last sync, which
    /// the live branch has usually moved past, and the review's base is anchored
    /// to that older commit. Fetching the branch brings the commit down as one
    /// of its ancestors. Fails when this SCM cannot speak the protocol `source`
    /// names, or when the reported commit is absent from the fetched branch (a
    /// force-push or rebase of the target since the last sync).
    async fn fetch_base(&self, source: &FetchSource, session: SessionId) -> Result<RevisionId>;

    /// List the repository's remotes, one per remote name with the URL git
    /// fetches from (a remote configured with several URLs reports its first).
    async fn remotes(&self) -> Result<Vec<Remote>>;

    /// Whether the working tree matches its committed state, with no staged or
    /// unstaged changes to tracked files. Untracked files are ignored, since
    /// they belong to no commit and do not change what publishing a branch
    /// sends.
    async fn working_tree_is_clean(&self) -> Result<bool>;

    /// The commit `branch` resolves to on `remote`, or `None` when the remote
    /// has no branch of that name.
    async fn remote_branch(&self, remote: &str, branch: &str) -> Result<Option<RevisionId>>;

    /// The remote branch the checked-out branch tracks, or `None` when it has no
    /// upstream or the head is detached.
    async fn current_upstream(&self) -> Result<Option<TrackingBranch>>;

    /// Publish `commit` to `remote` (the local name of the repository's remote
    /// for the forge host) as branch `branch`. Records tracking to the published
    /// branch when the checked-out branch has no upstream, leaving an existing
    /// upstream as the user configured it.
    async fn publish_branch(&self, remote: &str, branch: &str, commit: &RevisionId) -> Result<()>;

    /// Delete the session's pins, letting the fetched commits be garbage
    /// collected. The session's own files are untouched; discarding a session
    /// calls this to leave nothing behind in the repo. A pin that is already
    /// absent is not an error.
    async fn remove_pins(&self, session: SessionId) -> Result<()>;
}
