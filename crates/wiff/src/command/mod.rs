//! The `wiff` subcommands: one module per command, each owning its arguments
//! and its `run` entry point. This module wires them into the top-level
//! [`Command`] enum and holds the few helpers shared across commands.

mod comment;
mod description;
pub(crate) mod explore;
mod forge;
mod new;
mod refresh;
mod render;
mod resume;
mod session;
mod skill;
mod themes;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::Subcommand;
use tokio::io::AsyncReadExt;
use wiff_config::Config;
use wiff_core::record::{Author, AuthorKind, ScmSource, SessionHeader, SourceKind, TipRule};
use wiff_core::review::ReviewState;
use wiff_core::session::{active_session, data_dir, resolve_session_id, session_file};
use wiff_core::source::{GitRepo, HeadBranch, ScmRepo, head_branch};
use wiff_core::{
    BaseRuleset, CapturedDiff, DiffSource, GitSource, JjRepo, JjSource, ProjectIdentity, ScmType,
    capture_explore, explore_file_set,
};

use self::comment::CommentArgs;
use self::description::DescriptionArgs;
use self::explore::ExploreArgs;
use self::forge::ForgeArgs;
pub(crate) use self::forge::{connect_forge, reconcile_before_push};
use self::new::NewArgs;
use self::refresh::RefreshArgs;
use self::render::RenderArgs;
use self::resume::ResumeArgs;
use self::session::SessionArgs;

/// The top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a review session from a source and open it.
    New(NewArgs),
    /// Resume an existing review session.
    Resume(ResumeArgs),
    /// Manage sessions.
    Session(SessionArgs),
    /// Capture a new diff version into a session and rebase comments.
    Refresh(RefreshArgs),
    /// Manage the file set of an explore review.
    Explore(ExploreArgs),
    /// Add or manage comments.
    Comment(CommentArgs),
    /// Set or show the review's description.
    Description(DescriptionArgs),
    /// Render the review state for consumption.
    Render(RenderArgs),
    /// Mirror a pull request into a session and publish the review back.
    Forge(ForgeArgs),
    /// Write the agent skill file and print its path.
    SkillPath,
    /// List the bundled syntax theme names.
    Themes,
}

impl Command {
    /// Run the selected subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        match self {
            Command::New(args) => args.run().await,
            Command::Comment(args) => args.run().await,
            Command::Description(args) => args.run().await,
            Command::Render(args) => args.run(),
            Command::Session(args) => args.run(),
            Command::Refresh(args) => args.run().await,
            Command::Explore(args) => args.run(),
            Command::Resume(args) => args.run(),
            Command::Forge(args) => args.run().await,
            Command::SkillPath => skill::run(),
            Command::Themes => themes::run(),
        }
    }
}

/// Resolve the session file to act on: the one named by `session`, else the
/// active session for the project derived from the cwd (or forced by `project`).
fn resolve_session(session: Option<&str>, project: Option<&str>) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let identity = ProjectIdentity::for_dir_or_forced(&cwd, project)?;
    let base = data_dir()?;
    match session {
        Some(session) => {
            let id = resolve_session_id(&base, &identity.canonical, session)?;
            Ok(session_file(&base, &identity.canonical, id))
        }
        None => Ok(active_session(
            &base,
            &identity.canonical,
            identity.repo_root.as_deref(),
            identity.scm,
        )?),
    }
}

/// The author an action is attributed to: the default name for the kind acted
/// as -- an agent annotates as "assistant", a human as $USER -- honoring any
/// configured name, which an explicit `name` overrides in turn.
pub(crate) fn resolve_author(agent: bool, name: Option<String>) -> anyhow::Result<Author> {
    let kind = if agent {
        AuthorKind::Agent
    } else {
        AuthorKind::Human
    };
    let mut author = Config::load()?.author.resolve(kind);
    if let Some(name) = name {
        author.name = name;
    }
    Ok(author)
}

/// Which slice of a repository to capture, independent of the source-control
/// system it lives in.
#[derive(Debug, Clone)]
pub(crate) enum DiffSelection {
    /// The uncommitted working tree.
    WorkingCopy,
    /// The staged index against its base.
    Staged,
    /// The changes a named branch, change, or revision introduces.
    Change(String),
}

/// Capture `selection` from the repository at `root` using its detected `scm`.
/// Errors when the repository is of a kind wiff cannot yet capture from. The
/// captured diff may be empty (a clean working tree); whether an empty capture
/// is an error or a benign no-op is the calling command's decision. This is the
/// single point that turns a selection into a running SCM command; adding a new
/// source-control system means adding its arm here.
pub(crate) async fn capture_scm_diff(
    scm: Option<ScmType>,
    root: PathBuf,
    selection: DiffSelection,
    base: Option<BaseRuleset>,
) -> anyhow::Result<CapturedDiff> {
    let captured = match scm {
        Some(ScmType::Git) => git_source(root, selection, base).await?.capture().await?,
        Some(ScmType::Jujutsu) => jj_source(root, selection, base).await?.capture().await?,
        Some(other) => bail!(
            "{} is a {other} repository, which wiff cannot capture from yet; pipe a unified diff on stdin instead",
            root.display()
        ),
        None => bail!(
            "{} is not a recognized repository; pipe a unified diff on stdin instead",
            root.display()
        ),
    };
    Ok(captured)
}

/// A handle to the repository at `root` for the forge operations [`ScmRepo`]
/// names, dispatched on its detected `scm`. This is the single point that binds
/// a repository kind to its [`ScmRepo`] implementation; adding a new
/// source-control system means adding its arm here. Errors for a kind wiff
/// cannot yet drive through the trait, and for a directory with no known scm.
pub(crate) fn scm_repo(scm: Option<ScmType>, root: PathBuf) -> anyhow::Result<Box<dyn ScmRepo>> {
    match scm {
        Some(ScmType::Git) => Ok(Box::new(GitRepo::new(root))),
        Some(ScmType::Jujutsu) => Ok(Box::new(JjRepo::new(root))),
        Some(other) => bail!(
            "{} is a {other} repository, which wiff cannot drive yet",
            root.display()
        ),
        None => bail!("{} is not a recognized repository", root.display()),
    }
}

/// Build the git source for `selection`. An explicit `base` overrides the
/// default for the selection: a working-tree or staged review otherwise pins its
/// base at the current commit, while a named change reviews against its first
/// parent.
async fn git_source(
    root: PathBuf,
    selection: DiffSelection,
    base: Option<BaseRuleset>,
) -> anyhow::Result<GitSource> {
    Ok(match selection {
        DiffSelection::WorkingCopy => {
            let base = git_pinned_or(base, &root).await?;
            GitSource::working_copy(root, base)
        }
        DiffSelection::Staged => {
            let base = git_pinned_or(base, &root).await?;
            GitSource::index(root, base)
        }
        DiffSelection::Change(change) => {
            // The default first-parent base shows a merge's net change onto the
            // mainline (everything the merged branch brought in), rather than
            // git show's combined diff, which for a clean merge is empty. An
            // explicit --base overrides this.
            let base = base.unwrap_or_else(|| BaseRuleset::new("parent(@)"));
            GitSource::change(root, base, change).await?
        }
    })
}

/// Build the jj source for `selection`.  An explicit `base` overrides the
/// default: a working-copy review otherwise pins its base at `@-`, while a
/// named change reviews against its first parent.  jj has no staging area to
/// diff against. This function returns a clear error for `Staged` rather than
/// attempting a diff with nothing to produce it.
async fn jj_source(
    root: PathBuf,
    selection: DiffSelection,
    base: Option<BaseRuleset>,
) -> anyhow::Result<JjSource> {
    Ok(match selection {
        DiffSelection::WorkingCopy => {
            let base = jj_pinned_or(base, &root).await?;
            JjSource::working_copy(root, base)
        }
        DiffSelection::Staged => {
            bail!("jj has no staging area; use `wiff new` without `--cached`")
        }
        DiffSelection::Change(change) => {
            let base = base.unwrap_or_else(|| BaseRuleset::new("parent(@)"));
            JjSource::change(root, base, change).await?
        }
    })
}

/// Returns the explicit `base`, or the git ruleset pinning the review at HEAD.
async fn git_pinned_or(base: Option<BaseRuleset>, root: &Path) -> anyhow::Result<BaseRuleset> {
    match base {
        Some(base) => Ok(base),
        None => Ok(GitSource::pinned_base_at_head(root.to_path_buf()).await?),
    }
}

/// Returns the explicit `base`, or the jj ruleset pinning the review at `@-`.
async fn jj_pinned_or(base: Option<BaseRuleset>, root: &Path) -> anyhow::Result<BaseRuleset> {
    match base {
        Some(base) => Ok(base),
        None => Ok(JjSource::pinned_base_at_head(root.to_path_buf()).await?),
    }
}

/// Recapture a session's diff from the source recorded in its header, or `None`
/// when that source is a one-shot diff (piped on stdin) that cannot be
/// regenerated. Both `wiff refresh` and the in-TUI refresh flow through here, so
/// the mapping from a recorded source back to a live capture lives in one place.
/// The whole `state` is taken, not just its header, because an explore refresh
/// re-reads the file set recorded in the latest version rather than a range
/// recorded in the header.
pub(crate) async fn recapture_diff(state: &ReviewState) -> anyhow::Result<Option<CapturedDiff>> {
    let header = &state.session;
    let ScmSource {
        scm,
        base,
        tip,
        branch_hint,
    } = match &header.source {
        SourceKind::Scm(scm_source) => scm_source.clone(),
        SourceKind::Stdin | SourceKind::Unknown => return Ok(None),
        SourceKind::Forge => bail!("a forge session cannot yet be recaptured"),
        // An explore capture re-reads the files recorded in the latest version
        // from disk. A vanished file drops out of the capture and its comments
        // go outdated; there is no branch or base to guard, so switching
        // branches while reading code never blocks a refresh.
        SourceKind::Explore => return Ok(Some(recapture_explore(state))),
    };
    let root = header
        .repo_root
        .clone()
        .context("the session records no repository root, so its diff cannot be recaptured")?;
    match scm {
        ScmType::Git | ScmType::Jujutsu => {}
        other => bail!("wiff cannot yet recapture a {other} session"),
    }
    // A working-tree or index recapture reads whatever the repository root has
    // checked out now. The pinned base was chosen against the branch the session
    // was created on, so recapturing on a different branch would diff that base
    // against an unrelated working tree. Refuse unless the branch context still
    // matches the one recorded at creation, treating a detached-created session
    // that has since moved onto a branch as just such a mismatch. A committed tip
    // resolves the same revision regardless of what is checked out and needs no
    // guard, and neither does a root base, which is the same empty tree on every
    // branch (the review of a repository with no commits yet).
    if matches!(tip, TipRule::WorkingCopy | TipRule::Index) && !base.reviews_from_root() {
        match head_branch(Path::new(&root), scm) {
            HeadBranch::On(now) if branch_hint.as_deref() == Some(now.as_str()) => {}
            HeadBranch::Detached if branch_hint.is_none() => {}
            HeadBranch::Unknown => bail!(
                "wiff could not determine which branch the working copy is on \
                 (is the scm installed, and is this a valid repository?). Try again \
                 once the scm is reachable."
            ),
            HeadBranch::On(now) => {
                return Err(branch_context_moved(
                    branch_hint.as_deref(),
                    &format!("branch `{}`", strip_refs_heads_prefix(&now)),
                ));
            }
            HeadBranch::Detached => {
                return Err(branch_context_moved(
                    branch_hint.as_deref(),
                    "a detached HEAD",
                ));
            }
        }
    }
    let captured = match scm {
        ScmType::Git => {
            let source = match tip {
                TipRule::WorkingCopy => GitSource::working_copy(root, base),
                TipRule::Index => GitSource::index(root, base),
                other => GitSource::revision(root, base, other),
            };
            source.capture().await?
        }
        ScmType::Jujutsu => {
            let source = match tip {
                TipRule::WorkingCopy => JjSource::working_copy(root, base),
                TipRule::Index => {
                    bail!("jj has no staging area; use `wiff new` without `--cached`")
                }
                other => JjSource::revision(root, base, other),
            };
            source.capture().await?
        }
        // Already rejected above.
        other => bail!("wiff cannot yet recapture a {other} session"),
    };
    Ok(Some(captured))
}

/// The error refusing a recapture because the working copy's branch context has
/// moved away from the one the session was created on. `created_on` is the full
/// ref recorded at creation, or `None` when the session recorded no branch (its
/// HEAD was detached, or it predates branch recording); `now` is a
/// ready-phrased description of the current context.
fn branch_context_moved(created_on: Option<&str>, now: &str) -> anyhow::Error {
    // Only a session that recorded a branch can name one to return to. Without a
    // recorded branch there is no checkout that would restore the original
    // context, so the only remedy offered is to start a fresh session.
    match created_on.map(strip_refs_heads_prefix) {
        Some(branch) => anyhow::anyhow!(
            "This review session was created from a commit based on branch \
             `{branch}` but the working copy is currently checked out on a commit \
             based on {now}.\n\n\
             You either need to switch the working copy back to branch `{branch}` \
             to refresh the review, or quit this session and start (or resume) a \
             session from the current state of the repo if that is what you wish \
             to review."
        ),
        None => anyhow::anyhow!(
            "This review session was created without a recorded branch but the \
             working copy is currently checked out on a commit based on {now}.\n\n\
             To refresh against a different state, quit this session and start (or \
             resume) a session from the current state of the repo."
        ),
    }
}

/// Re-read the explore session's file set from disk into a fresh all-context
/// capture, for detecting whether the files on disk have moved on. The lock-held
/// widen path is what a refresh writes through; this is the advisory read.
pub(crate) fn recapture_explore(state: &ReviewState) -> CapturedDiff {
    let paths = explore_file_set(state);
    capture_explore(&explore_root(&state.session), &paths).captured
}

/// The directory an explore session's paths are read under: the repository root
/// when the session has one, else the directory it was created from, canonical
/// so a path normalized against it and later joined to it resolve to the same
/// file even when the root is reached through a symlink.
pub(crate) fn explore_root(header: &SessionHeader) -> PathBuf {
    let literal = Path::new(header.repo_root.as_ref().unwrap_or(&header.cwd));
    literal
        .canonicalize()
        .unwrap_or_else(|_| literal.to_path_buf())
}

/// A ref name with its `refs/heads/` prefix dropped for display, leaving the
/// bare branch name; any other ref form is returned unchanged.
fn strip_refs_heads_prefix(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

/// Read content piped on stdin, returning `None` when stdin is a terminal or
/// carries nothing. Only content actually piped counts; a non-terminal but
/// empty stdin (a redirect, or a non-interactive harness) yields `None` so a
/// caller can fall back rather than treat it as an empty input.
async fn read_piped_stdin() -> anyhow::Result<Option<String>> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    // Warn an interactive user that we are about to block on their input, so a
    // command awaiting a terminal-less stdin does not look hung.
    if std::io::stderr().is_terminal() {
        eprintln!("Reading from stdin...");
    }
    let mut text = String::new();
    tokio::io::stdin()
        .read_to_string(&mut text)
        .await
        .context("could not read from stdin")?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}
