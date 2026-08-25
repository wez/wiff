//! `wiff forge`: mirror a pull request into a session and publish the review
//! back to its host. The credential overrides and the step that turns a pull
//! request's host into a connected adapter are common to every subcommand, so
//! they live here; each subcommand owns the round-trips it drives through that
//! adapter.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand};
use wiff_config::Config;
use wiff_core::record::{Author, ForgeUrl, SourceKind, TipRule};
use wiff_core::session::{
    data_dir, forge_bound_sessions, resolve_session_id, session_binding, session_bound_to,
    session_file,
};
use wiff_core::source::CapturedDiff;
use wiff_core::{
    BaseRuleset, DiffSource, GitSource, JjSource, ProjectIdentity, ScmRepo, ScmType, SessionId,
    SessionLog,
};
use wiff_forge::{
    DeclinedWrite, FetchedPullRequest, Forge, GithubForge, ImportRequest, PushOutcome,
    ResyncOutcome, TokenOverride, assemble_diff, import_pull_request, push, resolve_token,
    resync_pull_request, select_pull_request_remote,
};

use super::{resolve_author, scm_repo};
use crate::tui;

/// Arguments for `wiff forge`.
#[derive(Debug, Args)]
pub struct ForgeArgs {
    #[command(flatten)]
    token: ForgeToken,
    #[command(subcommand)]
    command: ForgeCommand,
}

impl ForgeArgs {
    /// Dispatch the selected `wiff forge` subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        let cli = self.token.overrides();
        match self.command {
            ForgeCommand::Pull(args) => args.run(&cli).await,
            ForgeCommand::Push(args) => args.run(&cli).await,
        }
    }
}

/// The credentials for the forge, given directly or as a file to read the
/// token from. The two are mutually exclusive.
#[derive(Debug, Args)]
struct ForgeToken {
    /// Read the forge token from this file, using its trimmed contents.
    #[arg(long)]
    forge_token_file: Option<PathBuf>,
    /// The forge token, given directly.
    #[arg(long, conflicts_with = "forge_token_file")]
    forge_token: Option<String>,
}

impl ForgeToken {
    /// The command-line token overrides these arguments express.
    fn overrides(&self) -> TokenOverride {
        TokenOverride {
            token_file: self.forge_token_file.clone(),
            token: self.forge_token.clone(),
        }
    }
}

/// The `wiff forge` subcommands.
#[derive(Debug, Subcommand)]
enum ForgeCommand {
    /// Fetch a pull request into a session and open it.
    Pull(PullArgs),
    /// Publish the local review to the bound pull request.
    Push(PushArgs),
}

/// Arguments for `wiff forge pull`.
#[derive(Debug, Args)]
struct PullArgs {
    /// The pull request to mirror: a number read against the repository's forge
    /// remote, or a full pull-request URL such as
    /// `https://github.com/wezterm/wezterm/pull/6185`.
    #[arg(value_name = "NUMBER|URL")]
    pr: String,
    /// Review the pull request in a fresh session even when one is already bound
    /// to it, instead of re-syncing that session.
    #[arg(long)]
    new_session: bool,
    /// The display name to attribute a re-sync's rebased comments to.
    #[arg(long)]
    author: Option<String>,
    /// Attribute a re-sync's rebased comments to an agent rather than a human.
    #[arg(long)]
    agent: bool,
}

impl PullArgs {
    /// Mirror the named pull request into a session and open it.
    async fn run(self, cli: &TokenOverride) -> anyhow::Result<()> {
        let config = Config::load()?;
        let cwd = std::env::current_dir().context("could not determine the current directory")?;
        let (url, forge) = resolve_target(&self.pr, &cwd, &config, cli).await?;
        let base = data_dir()?;
        let author = resolve_author(self.agent, self.author)?;
        let session =
            mirror_pull_request(forge.as_ref(), &url, &cwd, &base, author, self.new_session)
                .await?;
        tui::open(&session, &config, cli, false)
    }
}

/// Arguments for `wiff forge push`.
#[derive(Debug, Args)]
struct PushArgs {
    /// The pull request to publish to, when the repository holds reviews of
    /// several: a number read against the repository's forge remote, or a full
    /// pull-request URL. Omitted when the repository holds a single bound review.
    #[arg(value_name = "NUMBER|URL")]
    pr: Option<String>,
    /// The exact session to publish, by id. Names one fork when several
    /// reviews of the same pull request share a binding and a URL cannot tell
    /// them apart.
    #[arg(long, conflicts_with = "pr")]
    session: Option<String>,
    /// The display name to attribute the pull-first rebase's re-anchored
    /// comments to, and whose local comments are published.
    #[arg(long)]
    author: Option<String>,
    /// Act as an agent rather than a human: attribute the rebase to an agent and
    /// publish that agent's comments instead of the human reviewer's.
    #[arg(long)]
    agent: bool,
}

impl PushArgs {
    /// Publish the repository's bound review to its pull request.
    async fn run(self, cli: &TokenOverride) -> anyhow::Result<()> {
        let config = Config::load()?;
        let cwd = std::env::current_dir().context("could not determine the current directory")?;
        let identity = ProjectIdentity::for_dir(&cwd).map_err(|_| {
            anyhow!(
                "wiff forge push publishes a review from the repository holding it, but the \
                 current directory is not inside a repository"
            )
        })?;
        let root = identity
            .repo_root
            .clone()
            .expect("for_dir yields a repo root on success");
        let repo = scm_repo(identity.scm, root.clone())?;
        let base = data_dir()?;
        let (path, url, forge) = self
            .locate_review(&identity, &base, &cwd, &config, cli)
            .await?;
        let mut log = SessionLog::open(&path)?;
        let author = resolve_author(self.agent, self.author)?;
        let (resync, pushed) = push_bound_review(
            forge.as_ref(),
            repo.as_ref(),
            identity.scm,
            &mut log,
            &root,
            &url,
            author,
        )
        .await?;
        report_push(&url, &resync, &pushed);
        Ok(())
    }

    /// The session to publish, its bound pull request, and an adapter for that
    /// pull request's host. An id names one session outright; a pull request
    /// resolves to the most recent session bound to it; with neither given, the
    /// repository's single bound session is published, and it is an error to
    /// have none, or to have several without naming which.
    async fn locate_review(
        &self,
        identity: &ProjectIdentity,
        base: &Path,
        cwd: &Path,
        config: &Config,
        cli: &TokenOverride,
    ) -> anyhow::Result<(PathBuf, ForgeUrl, Box<dyn Forge>)> {
        if let Some(session) = &self.session {
            let id = resolve_session_id(base, &identity.canonical, session)?;
            let path = session_file(base, &identity.canonical, id);
            let url = session_binding(&path)?.ok_or_else(|| {
                anyhow!(
                    "session {id} is not bound to a pull request; pull one with `wiff forge \
                     pull` before pushing"
                )
            })?;
            let forge = connect_forge(config, &url.host(), cli)?;
            return Ok((path, url, forge));
        }
        if let Some(pr) = &self.pr {
            let (url, forge) = resolve_target(pr, cwd, config, cli).await?;
            let path = session_bound_to(base, &identity.canonical, &url)?.ok_or_else(|| {
                anyhow!(
                    "no session in this repository is bound to {}; pull it with `wiff forge \
                     pull` before pushing",
                    url.as_str()
                )
            })?;
            return Ok((path, url, forge));
        }
        let mut bound = forge_bound_sessions(base, &identity.canonical)?;
        match bound.len() {
            0 => bail!(
                "no session in this repository is bound to a pull request; pull one with `wiff \
                 forge pull` before pushing"
            ),
            1 => {
                let only = bound.pop().expect("one bound session");
                let forge = connect_forge(config, &only.url.host(), cli)?;
                Ok((only.path, only.url, forge))
            }
            _ => {
                let listing = bound
                    .iter()
                    .map(|session| format!("{} ({})", session.id, session.url.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "several sessions here are bound to pull requests: {listing}; name which to \
                     push with `wiff forge push --session <id>`"
                )
            }
        }
    }
}

/// Reconcile the bound pull request's remote state as of the pre-push fetch into
/// the session behind `log`, then publish `author`'s local review to it. Pulling
/// first rebases the local review onto that fetched state; a forge edit made
/// after this fetch but before the review is submitted is left for the next pull
/// to reconcile. Returns what the reconcile imported and what the publish sent.
async fn push_bound_review(
    forge: &dyn Forge,
    repo: &dyn ScmRepo,
    scm: Option<ScmType>,
    log: &mut SessionLog,
    root: &Path,
    url: &ForgeUrl,
    author: Author,
) -> anyhow::Result<(ResyncOutcome, PushOutcome)> {
    let resync = reconcile_before_push(forge, repo, scm, log, root, url, author.clone()).await?;
    let pushed = push(forge, log, url, &author).await?;
    Ok((resync, pushed))
}

/// Fetch the bound pull request and reconcile its state as of that fetch into
/// the session behind `log`, rebasing the local review onto it and attributing
/// the re-anchors to `author`. This is the pull half of a publish, split out so
/// the TUI can let the reviewer read what the reconcile pulled in before the
/// push half sends the review back.
pub(crate) async fn reconcile_before_push(
    forge: &dyn Forge,
    repo: &dyn ScmRepo,
    scm: Option<ScmType>,
    log: &mut SessionLog,
    root: &Path,
    url: &ForgeUrl,
    author: Author,
) -> anyhow::Result<ResyncOutcome> {
    let fetched = forge.fetch(url).await?;
    let source = prepare_source(repo, scm, root, &fetched, log.id()).await?;
    resync_pull_request(log, source.as_ref(), &fetched, author).await
}

/// Print what pushing to `url` did: first what its pull-first step reconciled
/// from the forge, then what publishing the review sent.
fn report_push(url: &ForgeUrl, resync: &ResyncOutcome, outcome: &PushOutcome) {
    if resync.refresh.is_some()
        || resync.comments > 0
        || resync.reviews > 0
        || resync.description_updated
    {
        println!("reconciled from {}", url.as_str());
        if resync.refresh.is_some() {
            println!("  captured a new diff version");
        }
        if resync.comments > 0 {
            println!("  {} comments updated from the forge", resync.comments);
        }
        if resync.reviews > 0 {
            println!("  {} reviews updated from the forge", resync.reviews);
        }
        if resync.description_updated {
            println!("  description updated from the forge");
        }
    }
    println!("pushed review to {}", url.as_str());
    let mut reported = false;
    let mut line = |count: usize, label: &str| {
        if count > 0 {
            println!("  {count} {label}");
            reported = true;
        }
    };
    line(outcome.created.len(), "comments created");
    line(outcome.edited.len(), "comment edits published");
    line(outcome.resolved.len(), "comment resolutions published");
    if outcome.verdict_submitted {
        println!("  verdict submitted");
        reported = true;
    }
    if outcome.description_published {
        println!("  description updated");
        reported = true;
    }
    for declined in &outcome.declined {
        match declined {
            DeclinedWrite::Resolution(comment) => {
                println!(
                    "  resolution of comment {comment} not propagated: the forge does not support it"
                );
            }
            DeclinedWrite::Description => {
                println!("  description update not propagated: the forge does not support it");
            }
        }
        reported = true;
    }
    if !reported {
        println!("  nothing to publish; the review was already up to date");
    }
}

/// The pull request `wiff forge pull` names: a bare number resolved against the
/// repository's forge remote, or a full URL that names the forge outright.
enum PullTarget {
    /// A pull-request number, meaningful only against a repository's remote.
    Number(u64),
    /// A full pull-request URL, standing on its own without a repository.
    Url(ForgeUrl),
}

/// Tell a full pull-request URL from a bare number. The command line is the only
/// place that reads a bare id as a number; below it the id is an opaque string,
/// and this is the point to revisit for a forge that names its pull requests some
/// other way.
fn classify_target(pr: &str) -> anyhow::Result<PullTarget> {
    // A "://" marks input the user meant as a URL: parse it and report why an
    // ill-formed one is rejected, rather than fall through and misreport it as
    // not a URL at all.
    if pr.contains("://") {
        return Ok(PullTarget::Url(ForgeUrl::parse(pr)?));
    }
    match pr.parse::<u64>() {
        Ok(number) => Ok(PullTarget::Number(number)),
        Err(_) => bail!("{pr} is neither a pull-request number nor a full pull-request URL"),
    }
}

/// Resolve the user's pull-request argument for fetching: a full URL names the
/// host itself, while a bare number is meaningful only against a repository and
/// is read through the configured forge remote its clone URLs name. Returns the
/// canonical URL and an adapter connected to its host.
async fn resolve_target(
    pr: &str,
    cwd: &Path,
    config: &Config,
    cli: &TokenOverride,
) -> anyhow::Result<(ForgeUrl, Box<dyn Forge>)> {
    match classify_target(pr)? {
        PullTarget::Url(url) => {
            let forge = connect_forge(config, &url.host(), cli)?;
            Ok((url, forge))
        }
        PullTarget::Number(number) => {
            let identity = ProjectIdentity::for_dir(cwd).map_err(|_| {
                anyhow!(
                    "a pull-request number is read against the repository's forge remote, but \
                     the current directory is not inside a repository; give a full pull-request \
                     URL instead"
                )
            })?;
            let root = identity
                .repo_root
                .clone()
                .expect("for_dir yields a repo root on success");
            let repo = scm_repo(identity.scm, root).map_err(|err| {
                anyhow!(
                    "a pull-request number is read against a git remote, but {err:#}; give a full \
                     pull-request URL instead"
                )
            })?;
            let remotes = repo.remotes().await?;
            let (remote, host) = select_pull_request_remote(&remotes, &config.forge)?;
            let forge = connect_forge(config, &host, cli)?;
            let url = forge.pull_request_url(&remote.url, &number.to_string())?;
            Ok((url, forge))
        }
    }
}

/// Mirror a fetched pull request into a session, returning the session file to
/// open. When the pull request belongs to the enclosing repository (one of its
/// remotes addresses it) the review diffs its commits fetched into that repo;
/// otherwise, whether there is no repository or none of its remotes match, the
/// review diffs the blobs the forge serves and keys the session on a bucket read
/// from the pull request URL. Either way a session already bound to the pull
/// request is re-synced in place, attributing the rebased comments to `author`,
/// unless `new_session` forces a fresh review alongside it.
async fn mirror_pull_request(
    forge: &dyn Forge,
    url: &ForgeUrl,
    cwd: &Path,
    base: &Path,
    author: Author,
    new_session: bool,
) -> anyhow::Result<PathBuf> {
    if let Ok(identity) = ProjectIdentity::for_dir(cwd)
        && belongs_to_repo(forge, url, &identity).await?
    {
        return mirror_into_repo(forge, url, cwd, base, author, new_session, identity).await;
    }
    mirror_without_repo(forge, url, cwd, base, author, new_session).await
}

/// Whether the pull request `url` belongs to the repository `identity` names: a
/// remote of that repository addresses the same repository the pull request
/// lives in. A checkout with no repository root, or none of whose remotes match,
/// does not belong.
async fn belongs_to_repo(
    forge: &dyn Forge,
    url: &ForgeUrl,
    identity: &ProjectIdentity,
) -> anyhow::Result<bool> {
    let Some(root) = identity.repo_root.clone() else {
        return Ok(false);
    };
    let repo = scm_repo(identity.scm, root)?;
    for remote in repo.remotes().await? {
        if forge.matches_remote(url, &remote.url)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Mirror the pull request into a session bound to the enclosing repository,
/// diffing its commits fetched into repo-owned refs. A session already bound to
/// the pull request is re-synced in place; otherwise a fresh one is imported.
async fn mirror_into_repo(
    forge: &dyn Forge,
    url: &ForgeUrl,
    cwd: &Path,
    base: &Path,
    author: Author,
    new_session: bool,
    identity: ProjectIdentity,
) -> anyhow::Result<PathBuf> {
    let root = identity
        .repo_root
        .clone()
        .expect("for_dir yields a repo root on success");
    let repo = scm_repo(identity.scm, root.clone())?;

    let fetched = forge.fetch(url).await?;
    let existing = if new_session {
        None
    } else {
        session_bound_to(base, &identity.canonical, url)?
    };
    match existing {
        Some(path) => {
            let mut log = SessionLog::open(&path)?;
            let source =
                prepare_source(repo.as_ref(), identity.scm, &root, &fetched, log.id()).await?;
            resync_pull_request(&mut log, source.as_ref(), &fetched, author).await?;
            Ok(path)
        }
        None => {
            let session = SessionId::new();
            let source =
                prepare_source(repo.as_ref(), identity.scm, &root, &fetched, session).await?;
            let request = ImportRequest {
                session,
                base,
                identity: &identity,
                cwd,
            };
            import_pull_request(source.as_ref(), &fetched, &request).await?;
            Ok(session_file(base, &identity.canonical, session))
        }
    }
}

/// Mirror the pull request into a session with no local checkout, keyed by the
/// project bucket the forge reads from its URL. The review diffs the base and
/// head blobs the forge serves; refreshing it re-fetches those blobs rather than
/// re-resolving against a repository.
async fn mirror_without_repo(
    forge: &dyn Forge,
    url: &ForgeUrl,
    cwd: &Path,
    base: &Path,
    author: Author,
    new_session: bool,
) -> anyhow::Result<PathBuf> {
    let identity = ProjectIdentity::for_forge(&forge.project_bucket(url)?);
    let fetched = forge.fetch(url).await?;
    let source = forge_diff_source(forge, &fetched).await?;
    let existing = if new_session {
        None
    } else {
        session_bound_to(base, &identity.canonical, url)?
    };
    match existing {
        Some(path) => {
            let mut log = SessionLog::open(&path)?;
            resync_pull_request(&mut log, &source, &fetched, author).await?;
            Ok(path)
        }
        None => {
            let session = SessionId::new();
            let request = ImportRequest {
                session,
                base,
                identity: &identity,
                cwd,
            };
            import_pull_request(&source, &fetched, &request).await?;
            Ok(session_file(base, &identity.canonical, session))
        }
    }
}

/// Fetch the pull request's changed files and capture the diff assembled from
/// their base and head contents as a forge source, recording the base and head
/// commits as the version's provenance.
async fn forge_diff_source(
    forge: &dyn Forge,
    fetched: &FetchedPullRequest,
) -> anyhow::Result<CapturedDiff> {
    let files = forge.fetch_changed_files(&fetched.url).await?;
    Ok(CapturedDiff {
        text: assemble_diff(&files),
        source: SourceKind::Forge,
        base_revision: Some(fetched.base_commit().clone()),
        base_tip_relative: false,
        head_revision: Some(fetched.head_commit().clone()),
    })
}

/// Fetch and pin the pull request's head and target-branch commits in the
/// repository at `root` under `session`, then build a source that diffs the head
/// against its merge-base with the target branch, the range showing the pull
/// request's own changes and not commits the target has moved on to. The target
/// tip is fetched rather than assumed present: once the target advances past the
/// fork point it is no longer reachable from the head.
async fn prepare_source(
    repo: &dyn ScmRepo,
    scm: Option<ScmType>,
    root: &Path,
    fetched: &FetchedPullRequest,
    session: SessionId,
) -> anyhow::Result<Box<dyn DiffSource>> {
    let head = repo.fetch_pinned(&fetched.head, session).await?;
    repo.fetch_base(&fetched.base, session).await?;
    let base = BaseRuleset::new(format!(
        "merge-base(name({}))",
        fetched.base_commit().as_str()
    ));
    let tip = TipRule::Pinned { revision: head };
    // At this point scm is always Git or Jujutsu: forge operations require a
    // recognised SCM and fail earlier (in scm_repo) for anything else.
    Ok(match scm {
        Some(ScmType::Jujutsu) => Box::new(JjSource::revision(root.to_path_buf(), base, tip)),
        _ => Box::new(GitSource::revision(root.to_path_buf(), base, tip)),
    })
}

/// Build the forge adapter for `host`: look it up in the effective forge table,
/// resolve the token from the command-line overrides or the host's configured
/// variables, and construct the adapter the host's provider names.
pub(crate) fn connect_forge(
    config: &Config,
    host: &str,
    cli: &TokenOverride,
) -> anyhow::Result<Box<dyn Forge>> {
    let row = config.forge.host(host).with_context(|| {
        format!(
            "no forge is configured for {host}; add a [forge.\"{host}\"] entry naming its provider"
        )
    })?;
    let provider = row
        .provider
        .as_deref()
        .with_context(|| format!("the forge entry for {host} names no provider"))?;
    // Match the provider before resolving the token: an adapter wiff cannot
    // build should say so rather than first demand a credential it will not use.
    match provider {
        "github" => {
            let token = resolve_token(&row, cli, |name| std::env::var(name).ok())?;
            Ok(Box::new(GithubForge::new(&token, row.api_base.as_deref())?))
        }
        "forgejo" => bail!("the forgejo forge adapter is not available yet"),
        other => bail!("host {host} names an unknown forge provider {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use time::OffsetDateTime;
    use wiff_core::ScmType;
    use wiff_core::record::{
        AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentTarget, Description,
        ExternalKind, ExternalRef, FORMAT_VERSION, ForgeId, RecordBody, RevisionId, ScmSource,
        SessionHeader, SourceKind, VersionNumber,
    };
    use wiff_core::review::ReviewState;
    use wiff_core::session::LockWait;
    use wiff_core::source::FetchSource;
    use wiff_diff::FileStatus;
    use wiff_forge::{
        ChangedFile, Content, FetchedDescription, ForgeHost, ForgeTable, NewPullRequest,
        OutgoingComment, OutgoingReview, SubmittedReview,
    };

    use super::*;
    use crate::testutil::{git, git_out};
    use ulid::Ulid;

    /// A config whose forge table is `table` and whose other fields are the
    /// defaults, for exercising `connect_forge` without a config file.
    fn config_with(table: ForgeTable) -> Config {
        Config {
            forge: table,
            ..Config::default()
        }
    }

    /// The token override that hands the token over directly, so the resolution
    /// never consults the environment.
    fn direct_token(token: &str) -> TokenOverride {
        TokenOverride {
            token_file: None,
            token: Some(token.to_string()),
        }
    }

    // Building the octocrab-backed adapter spawns a background service, so it
    // needs a tokio runtime even though no request is made.
    #[tokio::test]
    async fn a_github_host_builds_an_adapter() {
        let config = config_with(ForgeTable::default());
        connect_forge(&config, "github.com", &direct_token("t")).expect("github adapter");
    }

    #[test]
    fn an_unconfigured_host_is_reported_with_its_name() {
        let config = config_with(ForgeTable::default());
        let error = connect_forge(&config, "git.example.org", &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "no forge is configured for git.example.org; add a \
             [forge.\"git.example.org\"] entry naming its provider"
        );
    }

    #[test]
    fn a_forgejo_host_reports_the_adapter_is_unavailable() {
        let config = config_with(ForgeTable::default());
        // No token is supplied: an adapter wiff cannot build is reported before
        // any credential is demanded.
        let error = connect_forge(&config, "codeberg.org", &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "the forgejo forge adapter is not available yet"
        );
    }

    #[test]
    fn an_unknown_provider_is_reported_with_the_host() {
        let table = ForgeTable::from([(
            "git.example.org".to_string(),
            ForgeHost {
                provider: Some("bitbucket".to_string()),
                ..ForgeHost::default()
            },
        )]);
        let config = config_with(table);
        let error = connect_forge(&config, "git.example.org", &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "host git.example.org names an unknown forge provider \"bitbucket\""
        );
    }

    /// Render a classification outcome for a full-output assertion.
    fn classified(pr: &str) -> String {
        match classify_target(pr) {
            Ok(PullTarget::Number(number)) => format!("number {number}"),
            Ok(PullTarget::Url(url)) => format!("url {}", url.as_str()),
            Err(error) => format!("error: {error}"),
        }
    }

    #[test]
    fn classify_target_tells_a_url_from_a_number_and_rejects_the_rest() {
        let cases: Vec<String> = [
            "7",
            "https://github.com/octo/demo/pull/7",
            "http://",
            "octo/demo#7",
            "",
        ]
        .into_iter()
        .map(|pr| format!("{pr:?} -> {}", classified(pr)))
        .collect();
        wince::assert_eq!(
            cases.join("\n"),
            "\"7\" -> number 7\n\
             \"https://github.com/octo/demo/pull/7\" -> url https://github.com/octo/demo/pull/7\n\
             \"http://\" -> error: http:// is not an absolute http(s) URL with a host\n\
             \"octo/demo#7\" -> error: octo/demo#7 is neither a pull-request number nor a full \
             pull-request URL\n\
             \"\" -> error:  is neither a pull-request number nor a full pull-request URL"
        );
    }

    // Resolving a full URL neither reads a repository nor reaches the network,
    // but building the github adapter spawns a background service that needs a
    // tokio runtime. The cwd is unread on this path.
    #[tokio::test]
    async fn a_url_target_resolves_to_the_pull_request_url() {
        let config = config_with(ForgeTable::default());
        let (url, _forge) = resolve_target(
            "https://github.com/octo/demo/pull/7",
            Path::new("."),
            &config,
            &direct_token("t"),
        )
        .await
        .expect("resolve");
        wince::assert_eq!(
            url.as_str().to_string(),
            "https://github.com/octo/demo/pull/7".to_string()
        );
    }

    #[tokio::test]
    async fn a_number_target_resolves_against_the_repository_forge_remote() {
        let repo = tempfile::tempdir().expect("repo tempdir");
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/octo/demo.git",
            ],
        );
        let config = config_with(ForgeTable::default());
        let (url, _forge) = resolve_target("7", repo.path(), &config, &direct_token("t"))
            .await
            .expect("resolve");
        wince::assert_eq!(
            url.as_str().to_string(),
            "https://github.com/octo/demo/pull/7".to_string()
        );
    }

    #[tokio::test]
    async fn a_number_target_outside_a_repository_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_with(ForgeTable::default());
        let error = resolve_target("7", dir.path(), &config, &direct_token("t"))
            .await
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "a pull-request number is read against the repository's forge remote, but the \
             current directory is not inside a repository; give a full pull-request URL instead"
        );
    }

    // A discovered root that is not a git checkout (here a bare `.hg` marker)
    // cannot answer a number, since only git is driven for a remote lookup.
    #[tokio::test]
    async fn a_number_target_in_a_non_git_repository_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".hg")).expect("mark hg root");
        let config = config_with(ForgeTable::default());
        let error = resolve_target("7", dir.path(), &config, &direct_token("t"))
            .await
            .map(|_| ())
            .unwrap_err();
        let message = format!("{error:#}").replace(&dir.path().display().to_string(), "TMPDIR");
        wince::assert_eq!(
            message,
            "a pull-request number is read against a git remote, but TMPDIR is a hg repository, \
             which wiff cannot drive yet; give a full pull-request URL instead"
        );
    }

    /// A forge whose `fetch` hands back a fixed pull request and whose write
    /// operations are never reached, for driving the mirror orchestration
    /// without a network.
    struct FetchForge(FetchedPullRequest);

    #[async_trait]
    impl Forge for FetchForge {
        async fn fetch(&self, _pr: &ForgeUrl) -> anyhow::Result<FetchedPullRequest> {
            Ok(self.0.clone())
        }

        async fn fetch_changed_files(
            &self,
            _pr: &ForgeUrl,
        ) -> anyhow::Result<Vec<wiff_forge::ChangedFile>> {
            unreachable!("mirroring a pull request does not fetch changed files")
        }

        async fn submit_review(
            &self,
            _pr: &ForgeUrl,
            _review: &OutgoingReview,
        ) -> anyhow::Result<SubmittedReview> {
            unreachable!("mirroring a pull request does not submit a review")
        }

        async fn post_comment(
            &self,
            _pr: &ForgeUrl,
            _comment: &OutgoingComment,
        ) -> anyhow::Result<ExternalRef> {
            unreachable!("mirroring a pull request does not post comments")
        }

        async fn edit_comment(&self, _at: &ExternalRef, _body: &str) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not edit comments")
        }

        async fn set_resolved(&self, _at: &ExternalRef, _resolved: bool) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not resolve comments")
        }

        async fn set_description(
            &self,
            _pr: &ForgeUrl,
            _description: &Description,
        ) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not set a description")
        }

        async fn create_pull_request(&self, _req: &NewPullRequest) -> anyhow::Result<ForgeUrl> {
            unreachable!("mirroring a pull request does not open one")
        }

        fn pull_request_url(&self, _remote_url: &str, _id: &str) -> anyhow::Result<ForgeUrl> {
            unreachable!("mirroring a pull request does not resolve one by id")
        }

        fn project_bucket(&self, _pr: &ForgeUrl) -> anyhow::Result<String> {
            unreachable!("an in-repo mirror keys on the repository, not the URL")
        }

        fn matches_remote(&self, _pr: &ForgeUrl, _remote_url: &str) -> anyhow::Result<bool> {
            // This fake stands in for a pull request that belongs to the repo.
            Ok(true)
        }
    }

    /// A human author with `name`.
    fn human(name: &str) -> Author {
        Author {
            name: name.to_string(),
            kind: AuthorKind::Human,
        }
    }

    /// A github external ref of `kind` with identifier `id`.
    fn github_ref(kind: ExternalKind, id: &str) -> ExternalRef {
        ExternalRef {
            forge: ForgeId {
                provider: "github".to_string(),
                host: "github.com".to_string(),
            },
            kind,
            id: id.to_string(),
            url: None,
        }
    }

    /// A pull request whose head is the `pr` branch of the git repository at
    /// `origin` and whose target is that repository's `main` at `base_commit`,
    /// with a description and no comments or reviews.
    fn fetched_pull_request(
        origin: &Path,
        base_commit: &str,
        head_commit: &str,
    ) -> FetchedPullRequest {
        FetchedPullRequest {
            url: ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url"),
            description: FetchedDescription {
                origin: github_ref(ExternalKind::Description, "7"),
                author: human("octo"),
                content: Description {
                    title: "PR title".to_string(),
                    body: "PR body".to_string(),
                },
                authored_at: OffsetDateTime::UNIX_EPOCH,
            },
            head: FetchSource::Git {
                url: origin.display().to_string(),
                git_ref: "refs/heads/pr".to_string(),
                commit: RevisionId(head_commit.to_string()),
            },
            base: FetchSource::Git {
                url: origin.display().to_string(),
                git_ref: "refs/heads/main".to_string(),
                commit: RevisionId(base_commit.to_string()),
            },
            comments: Vec::new(),
            reviews: Vec::new(),
        }
    }

    // A first pull imports a bound session from the fetched range and mirrors its
    // description; a second pull re-syncs that same session rather than
    // duplicating it, while --new-session deliberately forks a second review of
    // the same pull request.
    #[tokio::test]
    async fn mirroring_imports_then_resyncs_unless_a_new_session_is_forced() {
        let origin = tempfile::tempdir().expect("origin tempdir");
        let work = tempfile::tempdir().expect("work tempdir");
        let data = tempfile::tempdir().expect("data tempdir");

        git(origin.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(origin.path().join("f.txt"), "base\n").expect("write base");
        git(origin.path(), &["add", "f.txt"]);
        git(origin.path(), &["commit", "-q", "-m", "base"]);
        git(origin.path(), &["checkout", "-q", "-b", "pr"]);
        std::fs::write(origin.path().join("f.txt"), "base\nchange\n").expect("write change");
        git(origin.path(), &["commit", "-qa", "-m", "pr work"]);
        let head_commit = git_out(origin.path(), &["rev-parse", "HEAD"]);
        git(origin.path(), &["checkout", "-q", "main"]);

        git(
            work.path(),
            &["clone", "-q", &origin.path().display().to_string(), "."],
        );

        // Advance main past the fork point after the clone, so the target tip
        // the pull request names is absent from the working repo and must be
        // fetched to compute the merge-base.
        std::fs::write(origin.path().join("other.txt"), "later\n").expect("write other");
        git(origin.path(), &["add", "other.txt"]);
        git(origin.path(), &["commit", "-q", "-m", "advance main"]);
        let base_commit = git_out(origin.path(), &["rev-parse", "HEAD"]);

        let forge = FetchForge(fetched_pull_request(
            origin.path(),
            &base_commit,
            &head_commit,
        ));
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

        let first =
            mirror_pull_request(&forge, &url, work.path(), data.path(), human("wez"), false)
                .await
                .expect("first pull imports");
        let resynced =
            mirror_pull_request(&forge, &url, work.path(), data.path(), human("wez"), false)
                .await
                .expect("second pull resyncs");
        let forked =
            mirror_pull_request(&forge, &url, work.path(), data.path(), human("wez"), true)
                .await
                .expect("forced fresh session");

        let bucket = first.parent().expect("session bucket");
        let sessions = std::fs::read_dir(bucket)
            .expect("read bucket")
            .filter(|entry| {
                entry
                    .as_ref()
                    .expect("entry")
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "jsonl")
            })
            .count();
        let state = ReviewState::load(&first).expect("load imported session");
        let description = state
            .description
            .as_ref()
            .map(|d| format!("{} / {}", d.content.title, d.content.body))
            .unwrap_or_else(|| "(none)".to_string());
        let summary = format!(
            "resync reuses the bound session: {}\n\
             new-session forks a second: {}\n\
             sessions in bucket: {sessions}\n\
             bound to: {}\n\
             description: {description}\n\
             diff versions: {}",
            resynced == first,
            forked != first && forked.parent() == Some(bucket),
            state
                .session
                .forge
                .as_ref()
                .map(ForgeUrl::as_str)
                .unwrap_or("(none)"),
            state.versions.len(),
        );
        wince::assert_eq!(
            summary,
            "resync reuses the bound session: true\n\
             new-session forks a second: true\n\
             sessions in bucket: 2\n\
             bound to: https://github.com/octo/demo/pull/7\n\
             description: PR title / PR body\n\
             diff versions: 1"
                .to_string()
        );
    }

    /// A forge that serves a fixed pull request and a fixed set of changed files,
    /// for driving a repo-less mirror with no network and no local checkout.
    struct BlobForge {
        fetched: FetchedPullRequest,
        files: Vec<ChangedFile>,
    }

    #[async_trait]
    impl Forge for BlobForge {
        async fn fetch(&self, _pr: &ForgeUrl) -> anyhow::Result<FetchedPullRequest> {
            Ok(self.fetched.clone())
        }

        async fn fetch_changed_files(&self, _pr: &ForgeUrl) -> anyhow::Result<Vec<ChangedFile>> {
            Ok(self.files.clone())
        }

        async fn submit_review(
            &self,
            _pr: &ForgeUrl,
            _review: &OutgoingReview,
        ) -> anyhow::Result<SubmittedReview> {
            unreachable!("mirroring a pull request does not submit a review")
        }

        async fn post_comment(
            &self,
            _pr: &ForgeUrl,
            _comment: &OutgoingComment,
        ) -> anyhow::Result<ExternalRef> {
            unreachable!("mirroring a pull request does not post comments")
        }

        async fn edit_comment(&self, _at: &ExternalRef, _body: &str) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not edit comments")
        }

        async fn set_resolved(&self, _at: &ExternalRef, _resolved: bool) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not resolve comments")
        }

        async fn set_description(
            &self,
            _pr: &ForgeUrl,
            _description: &Description,
        ) -> anyhow::Result<()> {
            unreachable!("mirroring a pull request does not set a description")
        }

        async fn create_pull_request(&self, _req: &NewPullRequest) -> anyhow::Result<ForgeUrl> {
            unreachable!("mirroring a pull request does not open one")
        }

        fn pull_request_url(&self, _remote_url: &str, _id: &str) -> anyhow::Result<ForgeUrl> {
            unreachable!("mirroring a pull request does not resolve one by id")
        }

        fn project_bucket(&self, _pr: &ForgeUrl) -> anyhow::Result<String> {
            Ok("github.com/octo/demo".to_string())
        }

        fn matches_remote(&self, _pr: &ForgeUrl, _remote_url: &str) -> anyhow::Result<bool> {
            // This fake stands in for a pull request no local remote addresses.
            Ok(false)
        }
    }

    /// A `BlobForge` serving a pull request with no local checkout: the given
    /// changed `files` and a fixed description, head, and base.
    fn repoless_pull_request(files: &[ChangedFile]) -> BlobForge {
        BlobForge {
            fetched: FetchedPullRequest {
                url: ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url"),
                description: FetchedDescription {
                    origin: github_ref(ExternalKind::Description, "7"),
                    author: human("octo"),
                    content: Description {
                        title: "PR title".to_string(),
                        body: "PR body".to_string(),
                    },
                    authored_at: OffsetDateTime::UNIX_EPOCH,
                },
                head: FetchSource::Git {
                    url: "https://github.com/octo/demo.git".to_string(),
                    git_ref: "refs/pull/7/head".to_string(),
                    commit: RevisionId("headsha".to_string()),
                },
                base: FetchSource::Git {
                    url: "https://github.com/octo/demo.git".to_string(),
                    git_ref: "refs/heads/main".to_string(),
                    commit: RevisionId("basesha".to_string()),
                },
                comments: Vec::new(),
                reviews: Vec::new(),
            },
            files: files.to_vec(),
        }
    }

    /// One modified file whose head side becomes `after`.
    fn modified(after: &str) -> Vec<ChangedFile> {
        vec![ChangedFile {
            status: FileStatus::Modified,
            old_path: "mod.txt".to_string(),
            new_path: "mod.txt".to_string(),
            content: Content::Text {
                before: "old\n".to_string(),
                after: after.to_string(),
            },
        }]
    }

    // A pull request pulled outside any repository imports a bound session in a
    // bucket keyed on its URL, capturing the diff assembled from the fetched
    // blobs as a forge source. A second pull re-syncs that same session and,
    // because the forge now serves different contents, captures a fresh version
    // whose diff shows the new head side, confirming the re-fetch drives the
    // refresh rather than a stale snapshot.
    #[tokio::test]
    async fn a_repoless_pull_imports_a_forge_sourced_session_then_resyncs() {
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

        let first = mirror_pull_request(
            &repoless_pull_request(&modified("new\n")),
            &url,
            cwd.path(),
            data.path(),
            human("wez"),
            false,
        )
        .await
        .expect("first pull imports");
        let resynced = mirror_pull_request(
            &repoless_pull_request(&modified("newer\n")),
            &url,
            cwd.path(),
            data.path(),
            human("wez"),
            false,
        )
        .await
        .expect("second pull resyncs");

        let log = SessionLog::open(&first).expect("open imported session");
        let state = ReviewState::load(&first).expect("load imported session");
        let latest = state.versions.last().expect("a captured version").number;
        let v0 = log.read_diff(VersionNumber(0)).expect("read v0 diff");
        let latest_diff = log.read_diff(latest).expect("read latest diff");
        let bucket = first
            .parent()
            .and_then(|p| p.file_name())
            .expect("session bucket")
            .to_string_lossy()
            .into_owned();
        let source = match &state.session.source {
            SourceKind::Forge => "forge",
            SourceKind::Scm(_) => "scm",
            SourceKind::Stdin => "stdin",
            SourceKind::Explore => "explore",
            SourceKind::Unknown => "unknown",
        };
        let summary = format!(
            "bucket: {bucket}\n\
             source: {source}\n\
             repo root: {}\n\
             bound to: {}\n\
             resync reuses the session: {}\n\
             description: {} / {}\n\
             versions: {}\n\
             v0 diff:\n{v0}\
             latest diff (v{latest}):\n{latest_diff}",
            state.session.repo_root.as_deref().unwrap_or("(none)"),
            state
                .session
                .forge
                .as_ref()
                .map(ForgeUrl::as_str)
                .unwrap_or("(none)"),
            resynced == first,
            state
                .description
                .as_ref()
                .map(|d| d.content.title.clone())
                .unwrap_or_default(),
            state
                .description
                .as_ref()
                .map(|d| d.content.body.clone())
                .unwrap_or_default(),
            state.versions.len(),
        );
        wince::assert_eq!(
            summary,
            "bucket: github.com_octo_demo\n\
             source: forge\n\
             repo root: (none)\n\
             bound to: https://github.com/octo/demo/pull/7\n\
             resync reuses the session: true\n\
             description: PR title / PR body\n\
             versions: 2\n\
             v0 diff:\n\
             diff --git a/mod.txt b/mod.txt\n\
             --- a/mod.txt\n\
             +++ b/mod.txt\n\
             @@ -1 +1 @@\n\
             -old\n\
             +new\n\
             latest diff (v1):\n\
             diff --git a/mod.txt b/mod.txt\n\
             --- a/mod.txt\n\
             +++ b/mod.txt\n\
             @@ -1 +1 @@\n\
             -old\n\
             +newer\n"
                .to_string()
        );
    }

    // A pull request whose URL no remote of the enclosing repository addresses
    // is reviewed against the forge's blobs, not fetched into that unrelated
    // repository: the session is stored in the URL-keyed bucket with a forge
    // source and no repo root, where a repo-less pull of it would be too.
    #[tokio::test]
    async fn a_pull_in_an_unrelated_repository_falls_back_to_the_forge_bucket() {
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/unrelated.git",
            ],
        );
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

        let path = mirror_pull_request(
            &repoless_pull_request(&modified("new\n")),
            &url,
            repo.path(),
            data.path(),
            human("wez"),
            false,
        )
        .await
        .expect("pull falls back to a repo-less session");

        let state = ReviewState::load(&path).expect("load session");
        let bucket = path
            .parent()
            .and_then(|p| p.file_name())
            .expect("session bucket")
            .to_string_lossy()
            .into_owned();
        let source = match &state.session.source {
            SourceKind::Forge => "forge",
            SourceKind::Scm(_) => "scm",
            SourceKind::Stdin => "stdin",
            SourceKind::Explore => "explore",
            SourceKind::Unknown => "unknown",
        };
        let summary = format!(
            "bucket: {bucket}\n\
             source: {source}\n\
             repo root: {}\n\
             bound to: {}",
            state.session.repo_root.as_deref().unwrap_or("(none)"),
            state
                .session
                .forge
                .as_ref()
                .map(ForgeUrl::as_str)
                .unwrap_or("(none)"),
        );
        wince::assert_eq!(
            summary,
            "bucket: github.com_octo_demo\n\
             source: forge\n\
             repo root: (none)\n\
             bound to: https://github.com/octo/demo/pull/7"
                .to_string()
        );
    }

    // The in-repo/repo-less dispatch reads the repository's real remotes and
    // asks the real GitHub adapter whether any addresses the pull request: an
    // `origin` naming the same repository (here in scp-style, `.git`-suffixed
    // form) belongs, while one naming a different repository does not.
    #[tokio::test]
    async fn belongs_to_repo_matches_the_repository_remotes_against_the_adapter() {
        let matching = tempfile::tempdir().expect("matching tempdir");
        let unrelated = tempfile::tempdir().expect("unrelated tempdir");
        git(matching.path(), &["init", "-q"]);
        git(
            matching.path(),
            &["remote", "add", "origin", "git@github.com:octo/demo.git"],
        );
        git(unrelated.path(), &["init", "-q"]);
        git(
            unrelated.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/unrelated.git",
            ],
        );
        let forge = GithubForge::new("t", None).expect("adapter");
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");

        let identity_of = |dir: &Path| {
            ProjectIdentity::for_dir(dir).expect("a git checkout resolves an identity")
        };
        let report = format!(
            "matching remote belongs: {}\n\
             unrelated remote belongs: {}",
            belongs_to_repo(&forge, &url, &identity_of(matching.path()))
                .await
                .expect("classify matching"),
            belongs_to_repo(&forge, &url, &identity_of(unrelated.path()))
                .await
                .expect("classify unrelated"),
        );
        wince::assert_eq!(
            report,
            "matching remote belongs: true\n\
             unrelated remote belongs: false"
                .to_string()
        );
    }

    /// A forge that hands back a fixed pull request on fetch and binds every
    /// comment it is asked to publish to a predictable forge object, recording
    /// the ULIDs it posted standalone. The inline batch and the standalone posts
    /// both return objects, so a push can link them.
    struct PushForge {
        fetched: FetchedPullRequest,
        posted: Mutex<Vec<Ulid>>,
    }

    #[async_trait]
    impl Forge for PushForge {
        async fn fetch(&self, _pr: &ForgeUrl) -> anyhow::Result<FetchedPullRequest> {
            Ok(self.fetched.clone())
        }

        async fn fetch_changed_files(
            &self,
            _pr: &ForgeUrl,
        ) -> anyhow::Result<Vec<wiff_forge::ChangedFile>> {
            unreachable!("pushing a review does not fetch changed files")
        }

        async fn submit_review(
            &self,
            _pr: &ForgeUrl,
            review: &OutgoingReview,
        ) -> anyhow::Result<SubmittedReview> {
            let comments: BTreeMap<Ulid, ExternalRef> = review
                .comments
                .iter()
                .map(|c| {
                    (
                        c.comment,
                        github_ref(ExternalKind::ReviewComment, &format!("rc-{}", c.comment)),
                    )
                })
                .collect();
            Ok(SubmittedReview {
                review: github_ref(ExternalKind::Verdict, "review"),
                comments,
            })
        }

        async fn post_comment(
            &self,
            _pr: &ForgeUrl,
            comment: &OutgoingComment,
        ) -> anyhow::Result<ExternalRef> {
            self.posted.lock().expect("lock").push(comment.comment);
            Ok(github_ref(
                ExternalKind::ReviewComment,
                &format!("pc-{}", comment.comment),
            ))
        }

        async fn edit_comment(&self, _at: &ExternalRef, _body: &str) -> anyhow::Result<()> {
            unreachable!("this review has no linked comment to edit")
        }

        async fn set_resolved(&self, _at: &ExternalRef, _resolved: bool) -> anyhow::Result<()> {
            unreachable!("this review has no linked comment to resolve")
        }

        async fn set_description(
            &self,
            _pr: &ForgeUrl,
            _description: &Description,
        ) -> anyhow::Result<()> {
            unreachable!("the imported description already matches the forge")
        }

        async fn create_pull_request(&self, _req: &NewPullRequest) -> anyhow::Result<ForgeUrl> {
            unreachable!("pushing a review does not open a pull request")
        }

        fn pull_request_url(&self, _remote_url: &str, _id: &str) -> anyhow::Result<ForgeUrl> {
            unreachable!("pushing a review does not resolve one by id")
        }

        fn project_bucket(&self, _pr: &ForgeUrl) -> anyhow::Result<String> {
            unreachable!("pushing a review keys on the repository, not the URL")
        }

        fn matches_remote(&self, _pr: &ForgeUrl, _remote_url: &str) -> anyhow::Result<bool> {
            unreachable!("pushing a review does not test remotes")
        }
    }

    // A bound session's local comment is published to its pull request: the
    // pull-first resync finds nothing changed upstream, then the comment posts
    // and its forge object is linked back onto it.
    #[tokio::test]
    async fn pushing_a_bound_review_publishes_the_local_comment() {
        let origin = tempfile::tempdir().expect("origin tempdir");
        let work = tempfile::tempdir().expect("work tempdir");
        let data = tempfile::tempdir().expect("data tempdir");

        git(origin.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(origin.path().join("f.txt"), "base\n").expect("write base");
        git(origin.path(), &["add", "f.txt"]);
        git(origin.path(), &["commit", "-q", "-m", "base"]);
        git(origin.path(), &["checkout", "-q", "-b", "pr"]);
        std::fs::write(origin.path().join("f.txt"), "base\nchange\n").expect("write change");
        git(origin.path(), &["commit", "-qa", "-m", "pr work"]);
        let head_commit = git_out(origin.path(), &["rev-parse", "HEAD"]);
        git(origin.path(), &["checkout", "-q", "main"]);
        let base_commit = git_out(origin.path(), &["rev-parse", "HEAD"]);
        git(
            work.path(),
            &["clone", "-q", &origin.path().display().to_string(), "."],
        );

        let fetched = fetched_pull_request(origin.path(), &base_commit, &head_commit);
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");
        let import = FetchForge(fetched.clone());
        let path =
            mirror_pull_request(&import, &url, work.path(), data.path(), human("wez"), false)
                .await
                .expect("import binds a session");

        // A locally authored review-level comment, anchored against the imported
        // diff version, with no forge object yet.
        let comment = Ulid::from(42u128);
        let version = ReviewState::load(&path)
            .expect("load imported session")
            .latest_version()
            .expect("imported version")
            .number;
        let mut log = SessionLog::open(&path).expect("open session");
        let (mut lock, _records) = log.lock_and_sync(LockWait::Block).expect("lock");
        log.append(
            &mut lock,
            RecordBody::CommentEvent(CommentEvent {
                id: comment,
                author: human("wez"),
                authored_at: None,
                origin: None,
                kind: CommentEventKind::Create(CommentCreate {
                    target: CommentTarget::File {
                        file: "f.txt".to_string(),
                    },
                    version,
                    anchor: None,
                    body: "a note on the whole file".to_string(),
                    disposition: None,
                }),
            }),
        )
        .expect("append comment");
        drop(lock);

        let forge = PushForge {
            fetched,
            posted: Mutex::new(Vec::new()),
        };
        let repo = scm_repo(Some(ScmType::Git), work.path().to_path_buf()).expect("git repo");
        let (_resync, outcome) = push_bound_review(
            &forge,
            repo.as_ref(),
            Some(ScmType::Git),
            &mut log,
            work.path(),
            &url,
            human("wez"),
        )
        .await
        .expect("push publishes the review");

        let published = ReviewState::load(&path)
            .expect("reload session")
            .comments
            .iter()
            .find(|c| c.id == comment)
            .and_then(|c| c.origin.as_ref().map(|o| o.id.clone()))
            .unwrap_or_else(|| "(unlinked)".to_string());
        let join = |ids: &[Ulid]| {
            ids.iter()
                .map(Ulid::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let posted = join(&forge.posted.lock().expect("lock"));
        let summary = format!(
            "created: [{}]\n\
             edited: [{}]\n\
             resolved: [{}]\n\
             verdict submitted: {}\n\
             description published: {}\n\
             comment origin: {published}\n\
             posted standalone: [{posted}]",
            join(&outcome.created),
            join(&outcome.edited),
            join(&outcome.resolved),
            outcome.verdict_submitted,
            outcome.description_published,
        );
        wince::assert_eq!(
            summary,
            format!(
                "created: [{comment}]\n\
                 edited: []\n\
                 resolved: []\n\
                 verdict submitted: false\n\
                 description published: false\n\
                 comment origin: pc-{comment}\n\
                 posted standalone: [{comment}]"
            )
        );
    }

    // A repository whose only session never bound a pull request has nothing to
    // push to, so a bare push is refused by name rather than reaching the
    // network.
    #[tokio::test]
    async fn pushing_with_no_bound_session_is_refused() {
        let data = tempfile::tempdir().expect("data tempdir");
        let (_log, lock) = SessionLog::create(data.path(), "demo", |id| {
            RecordBody::Session(SessionHeader {
                id,
                version: FORMAT_VERSION,
                project: "demo".to_string(),
                repo_root: Some("/repos/demo".to_string()),
                cwd: "/repos/demo".to_string(),
                source: SourceKind::Scm(ScmSource {
                    scm: ScmType::Git,
                    base: BaseRuleset::new("ref(name(deadbeef))"),
                    tip: TipRule::WorkingCopy,
                    branch_hint: None,
                }),
                forge: None,
            })
        })
        .expect("create session");
        drop(lock);

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(PathBuf::from("/repos/demo")),
            scm: Some(ScmType::Git),
        };
        let args = PushArgs {
            pr: None,
            session: None,
            author: None,
            agent: false,
        };
        let error = args
            .locate_review(
                &identity,
                data.path(),
                Path::new("."),
                &config_with(ForgeTable::default()),
                &TokenOverride::default(),
            )
            .await
            .map(|_| ())
            .expect_err("an unbound session cannot be pushed");
        wince::assert_eq!(
            error.to_string(),
            "no session in this repository is bound to a pull request; pull one with `wiff \
             forge pull` before pushing"
                .to_string()
        );
    }

    /// A session header for `project` bound to `forge`, with an inert
    /// working-copy source, for populating a bucket without a repository.
    fn bucket_header(
        project: &str,
        forge: Option<ForgeUrl>,
    ) -> impl FnOnce(SessionId) -> RecordBody {
        let project = project.to_string();
        move |id| {
            RecordBody::Session(SessionHeader {
                id,
                version: FORMAT_VERSION,
                project: project.clone(),
                repo_root: Some("/repos/demo".to_string()),
                cwd: "/repos/demo".to_string(),
                source: SourceKind::Scm(ScmSource {
                    scm: ScmType::Git,
                    base: BaseRuleset::new("ref(name(deadbeef))"),
                    tip: TipRule::WorkingCopy,
                    branch_hint: None,
                }),
                forge,
            })
        }
    }

    // A bare push targets the session bound to a pull request, not the most
    // recently touched one: a later, unbound session in the same bucket (which
    // recency-based selection would pick) is passed over for the bound review.
    #[tokio::test]
    async fn a_bare_push_targets_the_bound_session_not_the_most_recent() {
        let data = tempfile::tempdir().expect("data tempdir");
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");
        let (bound, lock) = SessionLog::create(
            data.path(),
            "demo",
            bucket_header("demo", Some(url.clone())),
        )
        .expect("create bound session");
        drop(lock);
        // A later, unbound session: recency would prefer it, the binding must not.
        let (unbound, lock) = SessionLog::create(data.path(), "demo", bucket_header("demo", None))
            .expect("create unbound session");
        drop(lock);

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(PathBuf::from("/repos/demo")),
            scm: Some(ScmType::Git),
        };
        let args = PushArgs {
            pr: None,
            session: None,
            author: None,
            agent: false,
        };
        let (path, located, _forge) = args
            .locate_review(
                &identity,
                data.path(),
                Path::new("."),
                &config_with(ForgeTable::default()),
                &direct_token("t"),
            )
            .await
            .expect("the bound session is located");
        let summary = format!(
            "url: {}\nis the bound session: {}\nis the unbound session: {}",
            located.as_str(),
            path == bound.path(),
            path == unbound.path(),
        );
        wince::assert_eq!(
            summary,
            "url: https://github.com/octo/demo/pull/7\n\
             is the bound session: true\n\
             is the unbound session: false"
                .to_string()
        );
    }

    // Two reviews forked from one pull request (the `--new-session` workflow)
    // share a binding, so a bare push cannot choose between them and names both
    // by id for the user to pick with `--session`.
    #[tokio::test]
    async fn a_bare_push_across_same_pull_request_forks_names_each_by_id() {
        let data = tempfile::tempdir().expect("data tempdir");
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");
        // Choose the forks' ids so the second sorts ahead of the first, making
        // recency order them unambiguously and the listing deterministic.
        let (first, lock) = SessionLog::create_with_id(
            data.path(),
            "demo",
            "000000001".parse().expect("a valid session id"),
            bucket_header("demo", Some(url.clone())),
        )
        .expect("create first fork");
        drop(lock);
        let (second, lock) = SessionLog::create_with_id(
            data.path(),
            "demo",
            "000000002".parse().expect("a valid session id"),
            bucket_header("demo", Some(url.clone())),
        )
        .expect("create second fork");
        drop(lock);

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(PathBuf::from("/repos/demo")),
            scm: Some(ScmType::Git),
        };
        let args = PushArgs {
            pr: None,
            session: None,
            author: None,
            agent: false,
        };
        let error = args
            .locate_review(
                &identity,
                data.path(),
                Path::new("."),
                &config_with(ForgeTable::default()),
                &direct_token("t"),
            )
            .await
            .map(|_| ())
            .expect_err("same-pull-request forks cannot be chosen between");
        // The listing is most recent first, so the second fork, whose id sorts
        // ahead, is named before the first.
        wince::assert_eq!(
            error.to_string(),
            format!(
                "several sessions here are bound to pull requests: {} \
                 (https://github.com/octo/demo/pull/7), {} \
                 (https://github.com/octo/demo/pull/7); name which to push with `wiff forge push \
                 --session <id>`",
                second.id(),
                first.id(),
            )
        );
    }

    // Naming one fork by id publishes exactly that session, reaching a fork a
    // bare push or a URL (which resolves to the most recent) could not.
    #[tokio::test]
    async fn a_session_id_selects_one_same_pull_request_fork() {
        let data = tempfile::tempdir().expect("data tempdir");
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").expect("valid url");
        let (older, lock) = SessionLog::create(
            data.path(),
            "demo",
            bucket_header("demo", Some(url.clone())),
        )
        .expect("create older fork");
        drop(lock);
        let (newer, lock) = SessionLog::create(
            data.path(),
            "demo",
            bucket_header("demo", Some(url.clone())),
        )
        .expect("create newer fork");
        drop(lock);

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(PathBuf::from("/repos/demo")),
            scm: Some(ScmType::Git),
        };
        let args = PushArgs {
            pr: None,
            session: Some(older.id().to_string()),
            author: None,
            agent: false,
        };
        let (path, located, _forge) = args
            .locate_review(
                &identity,
                data.path(),
                Path::new("."),
                &config_with(ForgeTable::default()),
                &direct_token("t"),
            )
            .await
            .expect("the named fork is located");
        let summary = format!(
            "url: {}\nis the older fork: {}\nis the newer fork: {}",
            located.as_str(),
            path == older.path(),
            path == newer.path(),
        );
        wince::assert_eq!(
            summary,
            "url: https://github.com/octo/demo/pull/7\n\
             is the older fork: true\n\
             is the newer fork: false"
                .to_string()
        );
    }

    // A syntactically valid id that names no session file reports that plainly
    // rather than leaking the internal path through a raw I/O error.
    #[tokio::test]
    async fn a_session_id_naming_no_session_reports_it_plainly() {
        let data = tempfile::tempdir().expect("data tempdir");
        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(PathBuf::from("/repos/demo")),
            scm: Some(ScmType::Git),
        };
        let id: SessionId = "00000000z".parse().expect("valid session id");
        let args = PushArgs {
            pr: None,
            session: Some(id.to_string()),
            author: None,
            agent: false,
        };
        let error = args
            .locate_review(
                &identity,
                data.path(),
                Path::new("."),
                &config_with(ForgeTable::default()),
                &direct_token("t"),
            )
            .await
            .map(|_| ())
            .expect_err("an unknown session is refused");
        wince::assert_eq!(
            format!("{error:#}"),
            "no session in project demo matches id \"00000000z\"".to_string()
        );
    }
}
