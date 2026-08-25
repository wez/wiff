//! Opening a session in the review TUI.
//!
//! This is the bridge from the persisted session to the interactive review: it
//! loads the latest captured diff, renders it, and runs the terminal loop, then
//! keeps or removes the session according to how the reviewer chose to leave.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use wiff_config::{Config, OnExit};
use wiff_core::record::{
    Author, AuthorKind, CommentEventKind, ForgeUrl, RecordBody, SourceKind, VersionNumber,
};
use wiff_core::session::{SessionWatcher, read_records, remove_session, session_binding};
use wiff_core::source::ScmRepo;
use wiff_core::{
    AnchorFailures, CapturedDiff, LockWait, RefreshOutcome, ReviewState, ScmType, SessionLog,
    SidebandHash, capture_draft_anchors, compare_versions, explore_file_set, refresh_session,
    widen_explore,
};
use wiff_diff::decode_text;
use wiff_forge::{Forge, PushOutcome, ResyncOutcome, TokenOverride, push};
use wiff_tui::{
    App, CommentSync, CompareRequest, DiffView, Exit, ExitDefault, HighlightMode, Hooks, KeyHints,
    PublishStep, Review, Theme, run,
};

use crate::command::explore::to_slash;
use crate::command::{
    connect_forge, explore_root, recapture_diff, reconcile_before_push, scm_repo,
};

/// Parse unified diff `text` and expand its tabs to spaces at `tab_width` column
/// stops. Raw tabs are never rendered, since they would break the review's
/// fixed-column alignment.
fn parse_diff(text: &str, tab_width: usize) -> Result<wiff_diff::Diff, wiff_diff::ParseError> {
    let mut diff = wiff_diff::parse(text)?;
    diff.expand_tabs(tab_width);
    Ok(diff)
}

/// Open `session_path` in the review TUI, then keep or remove the session per
/// the reviewer's choice and the configured `on_exit` default. When
/// `offer_refresh` is set and recapturing the source would produce a diff
/// different from the latest captured version, a modal offers to refresh once
/// the existing state is on screen.
pub fn open(
    session_path: &Path,
    config: &Config,
    cli: &TokenOverride,
    offer_refresh: bool,
) -> anyhow::Result<()> {
    // The review takes the terminal from here on, so keep log output off its
    // alternate screen.
    crate::logging::silence_for_tui();
    let log = SessionLog::open(session_path)?;
    let state = ReviewState::load(session_path)?;
    let version = state
        .latest_version()
        .context("this session has no captured diff to review")?;
    let text = log.read_diff(version.number)?;
    let diff = parse_diff(&text, config.tab_width)?;

    // Build the UI with the appearance's baseline theme; when the appearance is
    // automatic, a startup terminal probe may switch to the light palette.
    let theme = Theme::named(config.theme.baseline_theme())
        .with_context(|| format!("unknown theme {:?}", config.theme.baseline_theme()))?;
    let auto_light = config
        .theme
        .probed_light_theme()
        .map(|name| Theme::named(name).with_context(|| format!("unknown light theme {name:?}")))
        .transpose()?;
    let sections = wiff_diff::SectionMatchers::new(&config.section)
        .context("a configured section pattern is not a valid regex")?;
    let generated =
        wiff_diff::GeneratedMatchers::new(&config.generated.names, &config.generated.markers)
            .context("a configured generated-file name pattern is not a valid glob")?;
    // Withdrawn comments are tombstones in the folded state; the review view
    // shows only the live ones.
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    let keymap = config.keymap()?;
    let view = DiffView::new(theme.clone())?
        .with_display_context(config.display_context)
        .with_min_fold(config.min_fold)
        .with_section_matchers(sections)
        .with_generated_file_matches(generated)
        .with_key_hints(KeyHints::from_keymap(&keymap));
    // Comments authored in the TUI are attributed to the human reviewer and
    // anchored against the diff version being reviewed.
    let author = config.author.resolve(AuthorKind::Human);
    // Under deterministic recording, highlight eagerly so each captured frame is
    // reproducible; interactive use defers highlighting to background threads.
    let highlight = if wiff_core::determinism::enabled() {
        HighlightMode::Eager
    } else {
        HighlightMode::Deferred
    };
    let review = Review::with_highlight_mode(
        view,
        diff,
        author.clone(),
        version.number.get(),
        comments,
        state.description.clone(),
        highlight,
    );
    let mut app = App::reviewing(review, 0, &theme)
        .with_exit_default(exit_default(config.on_exit))
        .with_add_files(matches!(state.session.source, SourceKind::Explore))
        .with_keymap(keymap.clone())
        .with_wrap_content(config.wrap_lines)
        .with_show_line_numbers(config.show_line_numbers)
        .with_diff_mode(config.diff_mode, config.side_by_side_min_width)
        .with_tab_width(config.tab_width)
        .with_nudge_to_detach(config.nudge_to_detach)
        .with_auto_light_theme(auto_light);
    // A forge-bound review refreshes by fetching the pull request over the
    // network, which blocks the loop, so give the app a modal to paint while it
    // runs. A local review recaptures a subprocess and shows no modal.
    if let Some(url) = &state.session.forge {
        app = app.with_refresh_modal(
            "Fetching",
            format!("Fetching the latest from {}", url.as_str()),
        );
    }
    let forge_bound = state.session.forge.is_some();
    // A resumed session whose source has moved on opens over the existing state
    // with a prompt to recapture it, rather than silently showing a stale diff.
    if offer_refresh && source_changed(&state) {
        app.offer_refresh();
    }

    // Refresh recaptures the diff and reloads the app in place; save commits the
    // pending drafts and keeps the review open. Either way a failure keeps the
    // review standing rather than tearing it down. A refresh failure (a refusal
    // to recapture, or an scm error) is often too long for the one-row status
    // line, so it is raised as a modal notice that wraps; save and compare report
    // in the status line.
    let refresh = |app: &mut App| {
        // A forge-bound review refreshes by fetching its pull request and
        // reconciling the forge state in; a local review recaptures its source.
        let result = if forge_bound {
            let connect = |url: &ForgeUrl| connect_forge(config, &url.host(), cli);
            refresh_forge_in_place(session_path, connect, &author, config.tab_width, app)
        } else {
            refresh_in_place(session_path, &author, config.tab_width, app)
        };
        if let Err(err) = result {
            app.show_notice("Refresh failed", err.to_string());
        }
    };
    // Publishing runs in confirmed phases the reviewer steps through: the key
    // press opens a prompt, accepting it reconciles the forge and (unless that
    // pulled in changes to read first) sends the review back. A failure at any
    // phase is raised as a modal notice, leaving the review standing.
    let publish = |app: &mut App, step: PublishStep| match step {
        PublishStep::Requested => match publish_target(session_path) {
            Ok(Some(url)) => app.offer_publish(url.as_str()),
            Ok(None) => app.show_notice(
                "Not a forge review",
                "This review is not bound to a pull request, so there is nothing to publish. \
                 Pull one with `wiff forge pull`.",
            ),
            Err(err) => app.show_notice("Publish failed", err.to_string()),
        },
        PublishStep::Reconcile => {
            let connect = |url: &ForgeUrl| connect_forge(config, &url.host(), cli);
            match publish_reconcile_in_place(session_path, connect, &author, config.tab_width, app)
            {
                Ok(Some(note)) => app.offer_publish_review(note),
                Ok(None) => {
                    let connect = |url: &ForgeUrl| connect_forge(config, &url.host(), cli);
                    if let Err(err) = publish_push_in_place(session_path, connect, &author, app) {
                        app.show_notice("Publish failed", err.to_string());
                    }
                }
                Err(err) => app.show_notice("Publish failed", err.to_string()),
            }
        }
        PublishStep::Publish => {
            let connect = |url: &ForgeUrl| connect_forge(config, &url.host(), cli);
            if let Err(err) = publish_push_in_place(session_path, connect, &author, app) {
                app.show_notice("Publish failed", err.to_string());
            }
        }
    };
    // Between key presses, pick up comments another actor (an agent, or a second
    // human) has committed to this session and fold them in. The watcher is a
    // cheap stat, so it costs nothing while the file is untouched.
    let mut watcher = SessionWatcher::new(session_path);
    let hooks = Hooks {
        refresh: Box::new(refresh),
        save: Box::new(|app: &mut App| {
            if let Err(err) = save_in_place(session_path, app) {
                app.set_message(format!("save failed: {err}"));
            }
        }),
        sync: Box::new(move |app: &mut App| {
            let Some(fingerprint) = watcher.changed() else {
                return false;
            };
            // Act only on a successful read. A read that fails because a line is
            // still being appended leaves the change unacknowledged, so the next
            // wakeup retries it once the line is whole.
            let Ok(summary) = reload_committed(session_path, app) else {
                return false;
            };
            watcher.acknowledge(fingerprint);
            if summary.is_empty() {
                return false;
            }
            app.set_message(sync_report(&summary));
            true
        }),
        compare: Box::new(|app: &mut App, request: CompareRequest| {
            if let Err(err) = compare_in_place(session_path, config.tab_width, app, request) {
                app.set_message(format!("compare failed: {err}"));
            }
        }),
        publish: Box::new(publish),
        list_files: Box::new(|app: &mut App| match addable_files(session_path) {
            Ok(files) => app.open_add_file_picker(files),
            Err(err) => app.show_notice("Add file failed", err.to_string()),
        }),
        add_file: Box::new(|app: &mut App, path: String| {
            if let Err(err) = add_file_in_place(session_path, &author, config.tab_width, app, path)
            {
                app.show_notice("Add file failed", err.to_string());
            }
        }),
    };

    let (exit, drafts) = run(app, keymap, hooks)?;
    resolve_exit(exit, session_path, drafts)
}

/// Reconstruct the diff the reviewer chose to compare against and show it in
/// place: an earlier version's after content on the left against the latest
/// version's, or the latest version's own captured diff when returning to it.
/// Reports what is shown in the status line.
fn compare_in_place(
    session_path: &Path,
    tab_width: usize,
    app: &mut App,
    request: CompareRequest,
) -> anyhow::Result<()> {
    let state = ReviewState::load(session_path)?;
    let latest = state
        .latest_version()
        .context("this session has no captured diff to compare")?
        .number;
    let log = SessionLog::open(session_path)?;
    let latest_diff = parse_diff(&log.read_diff(latest)?, tab_width)?;
    match request {
        CompareRequest::Latest => {
            app.show_comparison(latest_diff, None);
            app.set_message(format!("showing the latest diff (v{latest})"));
        }
        CompareRequest::Version(from) => {
            let from_diff = parse_diff(&log.read_diff(VersionNumber(from))?, tab_width)?;
            let comparison = compare_versions(&latest_diff, latest.get(), &from_diff, from);
            app.show_comparison(comparison.diff, Some((from, comparison.before_origin)));
            app.set_message(format!("comparing v{from} against v{latest}"));
        }
    }
    Ok(())
}

/// Recapture the session's diff as a new version, rebase its committed comments
/// and the reviewer's pending drafts onto it, and reload `app` over the full new
/// diff, reporting the tally in the status line and offering the
/// version-comparison list to narrow the view, opened on where the reviewer last
/// left comments. A no-op capture (nothing changed) says so instead.
fn refresh_in_place(
    session_path: &Path,
    author: &Author,
    tab_width: usize,
    app: &mut App,
) -> anyhow::Result<()> {
    // The reference version the reviewer was comparing against before the
    // recapture, or none when they were on the latest diff. The recapture resets
    // the view, but the picker below marks this as where they were.
    let viewing = app.comparing_from();
    let state = ReviewState::load(session_path)?;
    let prior_latest = state.latest_version().map(|v| v.number.get());
    let mut log = SessionLog::open(session_path)?;
    // An explore review re-reads its file set from the latest version under the
    // session lock; every other source recaptures its recorded range.
    let refreshed = if matches!(state.session.source, SourceKind::Explore) {
        let root = explore_root(&state.session);
        widen_explore(&mut log, &root, &[], author.clone(), LockWait::NonBlock)?
    } else {
        let captured = recapture(&state)?;
        refresh_session(&mut log, &captured, author.clone(), LockWait::NonBlock)?
    };
    let outcome = match refreshed {
        Some(outcome) => outcome,
        None => {
            let current = prior_latest.unwrap_or(0);
            app.set_message(format!("no changes since v{current}"));
            return Ok(());
        }
    };
    // The newest version at or before the pre-refresh latest where the reviewer
    // left comments is where their in-progress work sits; the picker offers
    // comparing against it.
    let last_commented = match prior_latest {
        Some(viewed) => last_commented_version(session_path, author, viewed)?,
        None => None,
    };

    let state = ReviewState::load(session_path)?;
    let latest = state
        .latest_version()
        .context("the refreshed session has no captured diff")?
        .number;
    let latest_diff = parse_diff(&log.read_diff(latest)?, tab_width)?;
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    app.refresh(latest_diff, comments, latest.get(), |authored_version| {
        parse_diff(&log.read_diff(VersionNumber(authored_version))?, tab_width).map_err(Into::into)
    })?;
    app.set_message(refresh_report(&outcome));
    // Offer the version list from the reviewer's pre-refresh perspective: their
    // prior view is marked and pre-selected, and cancelling keeps it against the
    // fresh capture, rather than switching under them mid-review. A base that
    // moved out from under the review leads the list, where the reviewer is
    // already choosing which range to look at.
    app.offer_compare_after_refresh(viewing, last_commented, base_shift_note(&outcome));
    Ok(())
}

/// The repository-relative paths an explore review could add: every file under
/// the review root, walked with `.gitignore` and `.ignore` conventions honored,
/// less the files already under review and any that cannot be read as text.
/// Dotfiles are offered (CI config and the like are prime review targets); only
/// the version control's own `.git` store is skipped. Sorted so the picker lists
/// them in a stable order.
fn addable_files(session_path: &Path) -> anyhow::Result<Vec<String>> {
    let state = ReviewState::load(session_path)?;
    let root = explore_root(&state.session);
    let under_review: HashSet<String> = explore_file_set(&state).into_iter().collect();
    let mut files: Vec<String> = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(&root).ok()?;
            let path = to_slash(relative);
            if under_review.contains(&path) {
                return None;
            }
            // Reading each candidate to reject binaries keeps the picker from
            // offering a path the add would refuse. A synchronous read fits the
            // human-scale trees explore reviews target; a large tree would want a
            // background walk (nucleo's worker pool) rather than this read.
            let bytes = std::fs::read(entry.path()).ok()?;
            decode_text(bytes).map(|_| path)
        })
        .collect();
    files.sort();
    Ok(files)
}

/// Widen the explore review with `path`, reload `app` over the new version, and
/// move the cursor to the added file. When the file was already under review the
/// widen writes nothing; the cursor still moves to it. Rebasing the existing
/// comments onto the new version is attributed to `author`.
fn add_file_in_place(
    session_path: &Path,
    author: &Author,
    tab_width: usize,
    app: &mut App,
    path: String,
) -> anyhow::Result<()> {
    let state = ReviewState::load(session_path)?;
    let root = explore_root(&state.session);
    let mut log = SessionLog::open(session_path)?;
    let widened = widen_explore(
        &mut log,
        &root,
        std::slice::from_ref(&path),
        author.clone(),
        LockWait::NonBlock,
    )?;
    let Some(outcome) = widened else {
        app.set_message(format!("{path} is already under review"));
        debug_assert!(
            app.move_to_file(&path),
            "a file under review must be locatable in the document"
        );
        return Ok(());
    };
    let state = ReviewState::load(session_path)?;
    let latest = state
        .latest_version()
        .context("the widened session has no captured diff")?
        .number;
    let latest_diff = parse_diff(&log.read_diff(latest)?, tab_width)?;
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    app.refresh(latest_diff, comments, latest.get(), |authored_version| {
        parse_diff(&log.read_diff(VersionNumber(authored_version))?, tab_width).map_err(Into::into)
    })?;
    app.set_message(format!("added {path} (v{})", outcome.version));
    debug_assert!(
        app.move_to_file(&path),
        "a just-widened file must be locatable in the reloaded document"
    );
    Ok(())
}

/// The newest diff version at or before `viewed` that `author` committed a
/// comment against, read from the session's records. Reads the authored version
/// from each comment record, which is fixed at commit time even as later
/// refreshes rebase the comment's anchor forward. None when they committed no
/// comment at or before that version.
fn last_commented_version(
    session_path: &Path,
    author: &Author,
    viewed: u32,
) -> anyhow::Result<Option<u32>> {
    Ok(read_records(session_path)?
        .into_iter()
        .filter_map(|record| match record.body {
            RecordBody::CommentEvent(event) if event.author == *author => match event.kind {
                CommentEventKind::Create(create) => Some(create.version.get()),
                _ => None,
            },
            _ => None,
        })
        .filter(|version| *version <= viewed)
        .max())
}

/// Commit the reviewer's pending drafts to the session log and reload the
/// review's committed comments over them, keeping the review open. Reports the
/// tally in the status line; a no-op when nothing is pending.
fn save_in_place(session_path: &Path, app: &mut App) -> anyhow::Result<()> {
    // Keep the drafts in the buffer until the commit is durable. A contended or
    // diverged log, or a torn read of the position check, fails the commit; the
    // buffer is then untouched and the reviewer can retry rather than lose work.
    let drafts = app.draft_records();
    if drafts.is_empty() {
        app.set_message("nothing to save".to_string());
        return Ok(());
    }
    let count = drafts.len();
    let anchor_failures = commit_drafts(session_path, drafts)?;
    app.clear_drafts();
    let changes = format!("{count} change{}", if count == 1 { "" } else { "s" });
    // The commit is durable once it returns; a reload failure here only leaves
    // the view stale until the next sync tick, so report the commit as done
    // rather than as a failed save that discarded nothing.
    let mut message = match reload_committed(session_path, app) {
        Ok(_) => format!("committed {changes}"),
        Err(err) => format!("committed {changes}; view refresh failed: {err}"),
    };
    // Report unanchored comments as a count: the status line is one row, and the
    // per-fault detail would bury the commit result. A comment still commits,
    // just without rebasing support. The causes are dropped here; a reviewer who
    // wants them sees the per-cause detail on the exit-commit path instead.
    if let Some(summary) = anchor_failures.summary() {
        message.push_str("; ");
        message.push_str(&summary);
    }
    app.set_message(message);
    Ok(())
}

/// The operation resolving a bound forge review, distinguishing the guidance a
/// review pulled outside its repository (which cannot yet reconcile from the
/// TUI) is given: which `wiff forge` subcommand to run from the repository
/// instead.
enum ForgeReviewAction {
    Refresh,
    Publish,
}

impl ForgeReviewAction {
    /// The error explaining that a review pulled outside its repository cannot
    /// reconcile from the TUI, naming the `wiff forge` subcommand to run from
    /// the repository instead.
    fn outside_repository_error(&self) -> &'static str {
        match self {
            Self::Refresh => {
                "refreshing a forge review pulled outside its repository is not supported from the \
                 review yet; use `wiff forge pull`"
            }
            Self::Publish => {
                "publishing a forge review pulled outside its repository is not supported from the \
                 review yet; use `wiff forge push`"
            }
        }
    }
}

/// A bound forge review resolved from its session on disk: the pull request it
/// is bound to, its repository, and a forge adapter for the round-trip.
struct ForgeReview {
    url: ForgeUrl,
    root: PathBuf,
    scm: Option<ScmType>,
    repo: Box<dyn ScmRepo>,
    forge: Box<dyn Forge>,
}

/// Resolve the bound forge review at `session_path`: its pull request, its
/// repository, and a forge adapter built by `connect`, which resolves the token
/// only now that a round-trip is underway. Fails when the review is unbound or
/// was pulled outside its repository, wording the latter for `action`.
fn resolve_forge_review(
    session_path: &Path,
    connect: impl Fn(&ForgeUrl) -> anyhow::Result<Box<dyn Forge>>,
    action: ForgeReviewAction,
) -> anyhow::Result<ForgeReview> {
    let state = ReviewState::load(session_path)?;
    let url = state
        .session
        .forge
        .clone()
        .context("this review is not bound to a pull request")?;
    let root = state
        .session
        .repo_root
        .clone()
        .context(action.outside_repository_error())?;
    let scm = match &state.session.source {
        SourceKind::Scm(scm_source) => Some(scm_source.scm),
        _ => None,
    };
    let root = PathBuf::from(root);
    let repo = scm_repo(scm, root.clone())?;
    let forge = connect(&url)?;
    Ok(ForgeReview {
        url,
        root,
        scm,
        repo,
        forge,
    })
}

/// Fetch the bound pull request and reconcile its forge state into the session,
/// reloading `app` over the result and reporting what was pulled in the status
/// line. The forge adapter is built by `connect` from the session's binding,
/// resolving its token only now that a fetch is underway. This is the pull half
/// of a publish on its own, letting the reviewer pull upstream changes without
/// sending anything back.
fn refresh_forge_in_place(
    session_path: &Path,
    connect: impl Fn(&ForgeUrl) -> anyhow::Result<Box<dyn Forge>>,
    author: &Author,
    tab_width: usize,
    app: &mut App,
) -> anyhow::Result<()> {
    let ForgeReview {
        url,
        root,
        scm,
        repo,
        forge,
    } = resolve_forge_review(session_path, connect, ForgeReviewAction::Refresh)?;
    let mut log = SessionLog::open(session_path)?;
    let outcome = block_on(reconcile_before_push(
        forge.as_ref(),
        repo.as_ref(),
        scm,
        &mut log,
        &root,
        &url,
        author.clone(),
    ))?;
    reload_after_reconcile(session_path, tab_width, &log, &outcome, app)?;
    match reconcile_note(&outcome) {
        Some(note) => app.set_message(note),
        None => app.set_message("already up to date with the forge".to_string()),
    }
    Ok(())
}

/// The pull request the session at `session_path` is bound to, or `None` when it
/// is not a forge review.
fn publish_target(session_path: &Path) -> anyhow::Result<Option<ForgeUrl>> {
    Ok(session_binding(session_path)?)
}

/// Commit the reviewer's pending drafts, then fetch the bound pull request and
/// reconcile its forge state into the session, reloading `app` over the result.
/// The forge adapter is built by `connect` from the session's binding, resolving
/// its token only now that a publish is underway. Returns a note describing what
/// the reconcile pulled in when it changed anything, for a prompt to read before
/// publishing, or `None` when the forge held nothing new.
fn publish_reconcile_in_place(
    session_path: &Path,
    connect: impl Fn(&ForgeUrl) -> anyhow::Result<Box<dyn Forge>>,
    author: &Author,
    tab_width: usize,
    app: &mut App,
) -> anyhow::Result<Option<String>> {
    // Resolve everything the publish needs -- the binding, the repository, the
    // forge and its token -- before committing any drafts, so an unpublishable
    // review (unbound, pulled outside its repository, or missing a token) fails
    // cleanly with the drafts still buffered rather than saving them and then
    // reporting a failure that contradicts the prompt the reviewer confirmed.
    let ForgeReview {
        url,
        root,
        scm,
        repo,
        forge,
    } = resolve_forge_review(session_path, connect, ForgeReviewAction::Publish)?;

    // The confirmation prompt covered saving along with publishing, so commit
    // the drafts now that the publish is sure to proceed; the push half sends
    // them. Reload them as committed immediately, so if the reconcile below
    // then fails the reviewer is left looking at their saved work rather than
    // an apparently empty buffer.
    let drafts = app.draft_records();
    if !drafts.is_empty() {
        commit_drafts(session_path, drafts)?;
        app.clear_drafts();
        reload_committed(session_path, app)?;
    }

    let mut log = SessionLog::open(session_path)?;
    let outcome = block_on(reconcile_before_push(
        forge.as_ref(),
        repo.as_ref(),
        scm,
        &mut log,
        &root,
        &url,
        author.clone(),
    ))?;
    reload_after_reconcile(session_path, tab_width, &log, &outcome, app)?;
    Ok(reconcile_note(&outcome))
}

/// Reload `app` over the session's reconciled state: the full recaptured diff
/// when the reconcile captured a new version, or just its committed comments
/// when only forge metadata changed.
fn reload_after_reconcile(
    session_path: &Path,
    tab_width: usize,
    log: &SessionLog,
    outcome: &ResyncOutcome,
    app: &mut App,
) -> anyhow::Result<()> {
    if outcome.refresh.is_none() {
        reload_committed(session_path, app)?;
        return Ok(());
    }
    let state = ReviewState::load(session_path)?;
    let latest = state
        .latest_version()
        .context("the reconciled session has no captured diff")?
        .number;
    let latest_diff = parse_diff(&log.read_diff(latest)?, tab_width)?;
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    app.refresh(latest_diff, comments, latest.get(), |authored_version| {
        parse_diff(&log.read_diff(VersionNumber(authored_version))?, tab_width).map_err(Into::into)
    })?;
    Ok(())
}

/// A note listing what a reconcile pulled in from the forge, or `None` when it
/// changed nothing.
fn reconcile_note(outcome: &ResyncOutcome) -> Option<String> {
    let mut parts = Vec::new();
    if outcome.refresh.is_some() {
        parts.push("a new diff version".to_string());
    }
    if outcome.comments > 0 {
        parts.push(format!(
            "{} updated {}",
            outcome.comments,
            plural(outcome.comments, "comment"),
        ));
    }
    if outcome.reviews > 0 {
        parts.push(format!(
            "{} updated {}",
            outcome.reviews,
            plural(outcome.reviews, "review"),
        ));
    }
    if outcome.description_updated {
        parts.push("a description update".to_string());
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("Reconciling pulled in {}.", parts.join(", ")))
}

/// Send the reconciled review to its bound pull request, reporting what was
/// published in the status line and reloading `app` to keep step with the log.
fn publish_push_in_place(
    session_path: &Path,
    connect: impl Fn(&ForgeUrl) -> anyhow::Result<Box<dyn Forge>>,
    author: &Author,
    app: &mut App,
) -> anyhow::Result<()> {
    let url =
        session_binding(session_path)?.context("this review is not bound to a pull request")?;
    let forge = connect(&url)?;
    let mut log = SessionLog::open(session_path)?;
    let outcome = block_on(push(forge.as_ref(), &mut log, &url, author))?;
    // A push records sync markers rather than visible comments, but reload so any
    // synced state the review shows keeps step with the log.
    let _ = reload_committed(session_path, app);
    app.set_message(push_report(&outcome));
    Ok(())
}

/// A status-line tally of what a push published, or a note that the review was
/// already up to date, with a count of any changes the forge declined.
fn push_report(outcome: &PushOutcome) -> String {
    let mut parts = Vec::new();
    if !outcome.created.is_empty() {
        parts.push(format!("{} created", outcome.created.len()));
    }
    if !outcome.edited.is_empty() {
        parts.push(format!("{} edited", outcome.edited.len()));
    }
    if !outcome.resolved.is_empty() {
        parts.push(format!("{} resolved", outcome.resolved.len()));
    }
    if outcome.verdict_submitted {
        parts.push("verdict submitted".to_string());
    }
    if outcome.description_published {
        parts.push("description updated".to_string());
    }
    let mut report = if parts.is_empty() {
        "published: already up to date".to_string()
    } else {
        format!("published: {}", parts.join(", "))
    };
    let declined = outcome.declined.len();
    if declined > 0 {
        report.push_str(&format!(
            "; {declined} {} the forge does not support",
            plural(declined, "change"),
        ));
    }
    report
}

/// Returns `word` pluralized by appending `s` unless `n` is one.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Block on `fut` from the review loop's tokio worker without standing up a
/// nested runtime.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

/// Reload the review's committed comments from the session log, dropping the
/// withdrawn ones, and report how they differ from what the app was showing.
fn reload_committed(session_path: &Path, app: &mut App) -> anyhow::Result<CommentSync> {
    let state = ReviewState::load(session_path)?;
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    Ok(app.reload_comments(comments, state.description.clone()))
}

/// A terse status note naming what another actor changed, joining only the parts
/// that are non-zero.
fn sync_report(summary: &CommentSync) -> String {
    let mut parts = Vec::new();
    if summary.added > 0 {
        parts.push(format!("{} added", summary.added));
    }
    if summary.changed > 0 {
        parts.push(format!("{} updated", summary.changed));
    }
    if summary.removed > 0 {
        parts.push(format!("{} removed", summary.removed));
    }
    if summary.description_changed {
        parts.push("description updated".to_string());
    }
    format!("synced: {}", parts.join(", "))
}

/// Whether recapturing the session's source would produce a diff different from
/// its latest captured version. False when the source cannot be recaptured (a
/// stdin diff) or the recapture fails, so opening a session is never blocked on
/// it.
fn source_changed(state: &ReviewState) -> bool {
    let Some(latest) = state.latest_version() else {
        return false;
    };
    let recaptured = block_on(recapture_diff(state));
    matches!(recaptured, Ok(Some(captured)) if SidebandHash::of(captured.text.as_bytes()) != latest.diff_hash)
}

/// Recapture the diff from the session's original source. A stdin source cannot
/// be reread inside the TUI, since stdin is now the terminal, so it is directed
/// to the `wiff refresh` command instead.
fn recapture(state: &ReviewState) -> anyhow::Result<CapturedDiff> {
    // The event loop runs on a tokio worker, so block on the async recapture
    // without standing up a nested runtime.
    let captured = block_on(recapture_diff(state))?;
    captured.context(
        "this session's diff came from stdin; refresh it with `wiff refresh` and a new piped diff",
    )
}

/// The status-line tally of a refresh: the captured version and how its comments
/// fared, led by a note when the review's base moved out from under it.
fn refresh_report(outcome: &RefreshOutcome) -> String {
    let total = outcome.exact + outcome.approximate + outcome.relocated + outcome.outdated;
    let tally = format!(
        "captured v{}; rebased {total} comment{}: {} exact, {} shifted, {} moved, {} outdated",
        outcome.version,
        if total == 1 { "" } else { "s" },
        outcome.exact,
        outcome.approximate,
        outcome.relocated,
        outcome.outdated,
    );
    match &outcome.base_shift {
        Some(shift) => format!("base moved {} -> {}; {tally}", shift.from, shift.to),
        None => tally,
    }
}

/// The advisory the post-refresh compare list leads with when the review's base
/// moved to a different commit, or none when it held still.
fn base_shift_note(outcome: &RefreshOutcome) -> Option<String> {
    outcome.base_shift.as_ref().map(|shift| {
        format!(
            "Base moved {} -> {}; the review now starts from a different commit.",
            shift.from, shift.to,
        )
    })
}

/// The TUI's exit default matching the configured `on_exit` policy.
fn exit_default(on_exit: OnExit) -> ExitDefault {
    match on_exit {
        OnExit::Keep => ExitDefault::Keep,
        OnExit::Remove => ExitDefault::Remove,
        OnExit::Prompt => ExitDefault::Prompt,
    }
}

/// Carry out the reviewer's chosen `exit`: commit the buffered drafts and keep
/// the session, keep it and drop the drafts, or remove it entirely.
fn resolve_exit(exit: Exit, session_path: &Path, drafts: Vec<RecordBody>) -> anyhow::Result<()> {
    match exit {
        Exit::Commit => {
            // Report the same count the interactive save shows, then the causes
            // stderr has room for that the one-row status line does not.
            let failures = commit_drafts(session_path, drafts)?;
            if let Some(summary) = failures.summary() {
                eprintln!("warning: {summary}");
                for cause in &failures.errors {
                    eprintln!("  {cause}");
                }
            }
            println!("kept session at {}", session_path.display());
        }
        Exit::Discard => {
            println!(
                "kept session at {} (drafts discarded)",
                session_path.display()
            );
        }
        Exit::Remove => {
            remove_session(session_path)?;
            println!("removed session");
        }
    }
    Ok(())
}

/// Commit the reviewer's buffered drafts, capturing each line comment's anchor
/// first. Returns the comments left unanchored by a damaged session and the
/// distinct faults behind them; those comments still commit, as bare locators.
fn commit_drafts(
    session_path: &Path,
    mut drafts: Vec<RecordBody>,
) -> anyhow::Result<AnchorFailures> {
    if drafts.is_empty() {
        return Ok(AnchorFailures::default());
    }
    let mut log = SessionLog::open(session_path)?;
    // Capture anchors before the append: the anchor must be part of the same
    // committed batch as the comment it belongs to. This reads each version's
    // sideband diff without the session lock; a written version's diff is fixed
    // once its record is appended, and if a concurrent removal takes it out from
    // under this read the comment simply commits as a bare locator.
    let anchor_failures = capture_draft_anchors(&log, &mut drafts);
    // Buffered drafts append as a batch that rejects a diverged file rather
    // than resyncing to it. The reviewer composed them against the view folded
    // at save time; failing the save on a concurrent write lets the view reload
    // and reconcile before the reviewer retries.
    log.append_all_locked(drafts)?;
    Ok(anchor_failures)
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;
    use wiff_core::comment::{delete_event, resolve_event};
    use wiff_core::record::{
        Author, AuthorKind, CommentCreate, CommentEvent, CommentEventKind, CommentTarget,
        RecordBody, RevisionId, ScmSource, SessionHeader, SourceKind, TipRule, VersionNumber,
    };
    use wiff_core::session::{SessionLog, SessionWatcher, read_records};
    use wiff_core::{
        BaseRuleset, CapturedDiff, DraftComment, LockWait, ProjectIdentity, RefreshOutcome,
        ReviewState, ScmType, SessionId, capture_explore, create_session,
    };
    use wiff_diff::{LineNo, Side};
    use wiff_forge::{DeclinedWrite, PushOutcome, ResyncOutcome};
    use wiff_tui::{
        Action, App, CommentSync, CompareRequest, DiffView, Key, KeyPress, Review, Theme,
    };

    use super::{
        addable_files, commit_drafts, compare_in_place, push_report, recapture, reconcile_note,
        refresh_in_place, refresh_report, reload_after_reconcile, reload_committed, save_in_place,
        source_changed, sync_report,
    };
    use crate::command::{DiffSelection, capture_scm_diff};
    use crate::testutil::{git, git_out};

    /// The human reviewer these tests attribute drafts to.
    fn wez() -> Author {
        Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        }
    }

    /// A bare review state with `source` and no versions, for exercising
    /// recapture routing.
    fn source_state(source: SourceKind) -> ReviewState {
        ReviewState {
            session: SessionHeader {
                id: "000000001".parse().expect("a valid session id"),
                version: wiff_core::record::FORMAT_VERSION,
                project: "demo".to_string(),
                repo_root: Some("/repos/demo".to_string()),
                cwd: "/repos/demo".to_string(),
                source,
                forge: None,
            },
            versions: Vec::new(),
            description: None,
            comments: Vec::new(),
            verdicts: Vec::new(),
            pushed_verdicts: Vec::new(),
        }
    }

    /// A session header for `id`, the first record of a fresh log.
    fn header(id: SessionId) -> RecordBody {
        RecordBody::Session(SessionHeader {
            id,
            version: wiff_core::record::FORMAT_VERSION,
            project: "demo".to_string(),
            repo_root: Some("/repos/demo".to_string()),
            cwd: "/repos/demo".to_string(),
            source: SourceKind::Stdin,
            forge: None,
        })
    }

    #[test]
    fn committing_drafts_appends_them_to_the_session_log_in_order() {
        let base = tempfile::tempdir().expect("tempdir");
        let (log, lock) = SessionLog::create(base.path(), "demo", header).expect("create");
        let path = log.path().to_path_buf();
        let id = log.id();
        drop(lock);
        drop(log);

        let drafts = vec![
            resolve_event(Ulid(1), wez(), true),
            delete_event(Ulid(2), wez()),
        ];
        commit_drafts(&path, drafts).expect("commit");

        // The header is followed by the two drafts in the order they were made;
        // the non-deterministic `at` timestamp is dropped from the comparison.
        let records = read_records(&path).expect("read");
        let got: Vec<(u64, RecordBody)> = records
            .into_iter()
            .map(|record| (record.seq.get(), record.body))
            .collect();
        wince::assert_eq!(
            got,
            vec![
                (0, header(id)),
                (1, resolve_event(Ulid(1), wez(), true)),
                (2, delete_event(Ulid(2), wez())),
            ]
        );
    }

    #[test]
    fn committing_no_drafts_leaves_the_log_untouched() {
        let base = tempfile::tempdir().expect("tempdir");
        let (log, lock) = SessionLog::create(base.path(), "demo", header).expect("create");
        let path = log.path().to_path_buf();
        let id = log.id();
        drop(lock);
        drop(log);

        commit_drafts(&path, Vec::new()).expect("commit");

        let records = read_records(&path).expect("read");
        let got: Vec<(u64, RecordBody)> = records
            .into_iter()
            .map(|record| (record.seq.get(), record.body))
            .collect();
        wince::assert_eq!(got, vec![(0, header(id))]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stdin_session_cannot_be_recaptured_in_the_tui() {
        // Stdin is the terminal once the TUI is open, so a stdin-sourced session
        // is directed to the `wiff refresh` command instead of being reread.
        // The recapture blocks on the runtime, so it needs one even though the
        // stdin arm never reaches git.
        let error = recapture(&source_state(SourceKind::Stdin)).unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "this session's diff came from stdin; refresh it with `wiff refresh` and a new piped diff"
                .to_string()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_refuses_when_the_working_copy_is_on_a_different_branch() {
        // A working copy session records the branch it was created on. Switching the
        // repository to another branch and refreshing would diff the pinned base
        // against an unrelated working tree, so recapture refuses with the two
        // branches named rather than capturing a misleading version.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q", "-b", "topic"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        // Switch to an unrelated branch, then attempt to recapture the session.
        git(repo.path(), &["checkout", "-q", "-b", "other"]);
        let state = ReviewState::load(&session_path).expect("load state");
        let error = recapture(&state).unwrap_err();
        wince::snapshot_display!(
            error,
            "This review session was created from a commit based on branch \
             `topic` but the working copy is currently checked out on a commit \
             based on branch `other`.\n\n\
             You either need to switch the working copy back to branch `topic` to \
             refresh the review, or quit this session and start (or resume) a \
             session from the current state of the repo if that is what you wish \
             to review."
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_captures_uncommitted_work_in_a_repository_with_no_commits() {
        // A repository with no commits reviews from the root (the empty tree),
        // the same on every branch, so a refresh is not gated on branch context
        // and recaptures whatever is uncommitted now, even from a branch other
        // than the one the session was created on.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");

        git(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        // Still no commit: switch to a different unborn branch, extend the
        // working copy, and add a file, then refresh. The branch has moved from
        // the recorded `main`, which a non-root base would refuse.
        git(repo.path(), &["checkout", "-q", "-b", "other"]);
        std::fs::write(repo.path().join("f.txt"), "alpha\nbeta\n").expect("write v1");
        std::fs::write(repo.path().join("g.txt"), "gamma\n").expect("write g");

        let state = ReviewState::load(&session_path).expect("load state");
        let recaptured = recapture(&state).expect("recapture");

        let empty_tree = RevisionId(git_out(
            repo.path(),
            &["hash-object", "-t", "tree", "/dev/null"],
        ));
        // The variable blob hashes on each `index` line are blanked so the whole
        // recaptured diff can be asserted.
        let normalized = recaptured
            .text
            .lines()
            .map(|line| {
                if line.starts_with("index ") {
                    "index HASHES"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        wince::assert_eq!(
            CapturedDiff {
                text: normalized,
                ..recaptured
            },
            CapturedDiff {
                text: "\
diff --git a/f.txt b/f.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,2 @@
+alpha
+beta
diff --git a/g.txt b/g.txt
new file mode 100644
index HASHES
--- /dev/null
+++ b/g.txt
@@ -0,0 +1 @@
+gamma"
                    .to_string(),
                source: SourceKind::Scm(ScmSource {
                    scm: ScmType::Git,
                    base: BaseRuleset::empty(),
                    tip: TipRule::WorkingCopy,
                    branch_hint: Some("refs/heads/other".to_string()),
                }),
                base_revision: Some(empty_tree),
                base_tip_relative: false,
                head_revision: None,
            }
        );
    }

    #[test]
    fn addable_files_omits_the_reviewed_set_and_binary_files() {
        // An explore session over one file. The candidate list a reviewer can add
        // covers the other text files under the root, sorted, with the file
        // already under review and the binary one both left out. A dotfile under
        // a dot-directory is offered (CI config is a review target); the `.git`
        // store is skipped.
        let root = tempfile::tempdir().expect("root tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        std::fs::create_dir_all(root.path().join("src")).expect("create src");
        std::fs::create_dir_all(root.path().join(".github/workflows")).expect("create workflows");
        std::fs::create_dir_all(root.path().join(".git")).expect("create git");
        std::fs::write(root.path().join("src/lib.rs"), "pub fn a() {}\n").expect("write lib");
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").expect("write main");
        std::fs::write(root.path().join("README.md"), "# demo\n").expect("write readme");
        std::fs::write(root.path().join(".github/workflows/ci.yml"), "on: push\n")
            .expect("write ci");
        std::fs::write(root.path().join(".git/config"), "[core]\n").expect("write git config");
        std::fs::write(root.path().join("logo.bin"), b"PNG\x00\x01\x02data").expect("write bin");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(root.path().to_path_buf()),
            scm: None,
        };
        let capture = capture_explore(root.path(), &["src/lib.rs".into()]);
        let log = create_session(data.path(), &identity, root.path(), &capture.captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let files = addable_files(&session_path).expect("list files");
        wince::assert_eq!(
            files,
            vec![
                ".github/workflows/ci.yml".to_string(),
                "README.md".to_string(),
                "src/main.rs".to_string(),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_refuses_a_detached_head_session_once_a_branch_is_checked_out() {
        // A working copy session captured on a detached head records no branch. Once
        // a branch is checked out the working tree is a different context than
        // the one the pinned base was chosen against, so recapture refuses rather
        // than diffing the base against an unrelated tree.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q", "-b", "topic"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // Detach HEAD onto the base commit, then make the working-tree change the
        // session captures.
        git(repo.path(), &["checkout", "-q", "--detach"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        // Check out a branch, then attempt to recapture the detached session.
        git(repo.path(), &["checkout", "-q", "topic"]);
        let state = ReviewState::load(&session_path).expect("load state");
        let error = recapture(&state).unwrap_err();
        wince::snapshot_display!(
            error,
            "This review session was created without a recorded branch but the \
             working copy is currently checked out on a commit based on branch \
             `topic`.\n\n\
             To refresh against a different state, quit this session and start (or \
             resume) a session from the current state of the repo."
        );
    }

    #[test]
    fn the_refresh_report_tallies_the_captured_version_and_comments() {
        let report = refresh_report(&RefreshOutcome {
            version: VersionNumber(3),
            exact: 2,
            approximate: 1,
            relocated: 1,
            outdated: 0,
            base_shift: None,
        });
        wince::assert_eq!(
            report,
            "captured v3; rebased 4 comments: 2 exact, 1 shifted, 1 moved, 0 outdated".to_string()
        );
    }

    #[test]
    fn the_refresh_report_leads_with_a_moved_base() {
        let report = refresh_report(&RefreshOutcome {
            version: VersionNumber(3),
            exact: 1,
            approximate: 0,
            relocated: 0,
            outdated: 0,
            base_shift: Some(wiff_core::BaseShift {
                from: wiff_core::record::RevisionId("oldbase".to_string()),
                to: wiff_core::record::RevisionId("newbase".to_string()),
            }),
        });
        wince::assert_eq!(
            report,
            "base moved oldbase -> newbase; captured v3; rebased 1 comment: 1 exact, 0 shifted, \
             0 moved, 0 outdated"
                .to_string()
        );
    }

    /// A refresh outcome that captured version `v`, standing in for a reconcile
    /// that pulled a new diff version; the rebase tallies are immaterial here.
    fn captured(v: u32) -> RefreshOutcome {
        RefreshOutcome {
            version: VersionNumber(v),
            exact: 0,
            approximate: 0,
            relocated: 0,
            outdated: 0,
            base_shift: None,
        }
    }

    #[test]
    fn a_reconcile_that_pulled_nothing_yields_no_note() {
        // Nothing arrived from the forge, so there is nothing to read before
        // publishing and the reconcile prompts nothing.
        let note = reconcile_note(&ResyncOutcome {
            refresh: None,
            comments: 0,
            reviews: 0,
            description_updated: false,
        });
        wince::assert_eq!(note, None);
    }

    #[test]
    fn a_reconcile_note_lists_every_kind_of_change_it_pulled() {
        // A reconcile that captured a new version and imported comments,
        // reviews, and a description update names each, pluralizing on count.
        let note = reconcile_note(&ResyncOutcome {
            refresh: Some(captured(2)),
            comments: 2,
            reviews: 1,
            description_updated: true,
        });
        wince::assert_eq!(
            note,
            Some(
                "Reconciling pulled in a new diff version, 2 updated comments, 1 updated review, \
                 a description update."
                    .to_string()
            )
        );
    }

    #[test]
    fn a_push_that_sent_nothing_reports_it_was_up_to_date() {
        // With no local changes to send, the push reports the review was already
        // in step with the forge.
        let report = push_report(&PushOutcome::default());
        wince::assert_eq!(report, "published: already up to date".to_string());
    }

    #[test]
    fn a_push_report_tallies_what_was_sent_and_what_the_forge_declined() {
        // A push that created, edited, and resolved comments, submitted a
        // verdict, and updated the description tallies each, and names the count
        // of changes the forge could not apply.
        let report = push_report(&PushOutcome {
            created: vec![Ulid(1), Ulid(2)],
            edited: vec![Ulid(3)],
            resolved: vec![Ulid(4)],
            verdict_submitted: true,
            description_published: true,
            declined: vec![DeclinedWrite::Description],
        });
        wince::assert_eq!(
            report,
            "published: 2 created, 1 edited, 1 resolved, verdict submitted, description updated; \
             1 change the forge does not support"
                .to_string()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reloading_after_a_reconcile_shows_the_recaptured_version_with_the_comment_rebased() {
        // A reconcile that pulled a new diff version reloads the review over the
        // freshly captured diff read from the log, rebasing the comment onto its
        // new line, exactly as a forge publish would after fetching upstream
        // commits. The session is advanced to v1 out of band to stand in for the
        // reconcile's recapture, then reload_after_reconcile is driven directly.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let mut log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        DraftComment {
            author: wez(),
            target: CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(4).unwrap(),
                end_line: LineNo::new(4).unwrap(),
            },
            body: "why delta?".to_string(),
            disposition: None,
        }
        .append(&mut log, LockWait::Block)
        .expect("attach comment");

        // The review opens over v0, the version the reviewer was reading before
        // the reconcile.
        let theme = Theme::dark();
        let v0 = ReviewState::load(&session_path).expect("load state");
        let v0_number = v0.latest_version().expect("a captured version").number;
        let v0_diff = wiff_diff::parse(&log.read_diff(v0_number).unwrap()).expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            v0_diff,
            wez(),
            v0_number.get(),
            v0.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        // A line is added at the top of the working tree and recaptured as v1,
        // sliding delta down; this stands in for the commits a reconcile fetches.
        std::fs::write(&file, "zero\nalpha\nbeta\ngamma\ndelta\n").expect("write v1");
        let recaptured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v1");
        let refresh = wiff_core::refresh_session(&mut log, &recaptured, wez(), LockWait::Block)
            .expect("advance to v1")
            .expect("the recapture changed the diff");

        reload_after_reconcile(
            &session_path,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &log,
            &ResyncOutcome {
                refresh: Some(refresh),
                comments: 0,
                reviews: 0,
                description_updated: false,
            },
            &mut app,
        )
        .expect("reload after reconcile");

        // The reloaded review shows the full recaptured v1 diff, with the comment
        // rebased above the added delta on its new line.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  f.txt\n",
            "@@ -1,3 +1,5 @@\n",
            "        1 + zero\n",
            "   1    2   alpha\n",
            "   2    3   beta\n",
            "   3    4   gamma\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why delta?                                                                    │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        5 +└delta\n",
            "---\n",
            "                                                                      1 open  0%\n",
        );
    }

    /// The plain text a reviewer sees on `app`'s screen at `width`: every visible
    /// line with its trailing highlight padding trimmed, then the status line.
    fn screen(app: &App, width: usize) -> String {
        let mut out = String::new();
        for line in app.visible(width) {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            out.push_str(text.trim_end());
            out.push('\n');
        }
        out.push_str("---\n");
        let status: String = app
            .status(width)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        out.push_str(status.trim_end());
        out.push('\n');
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refreshing_recaptures_the_working_tree_and_rebases_a_comment() {
        // A real git repo backs the session: a committed base, a working-tree
        // change captured as v0, a comment anchored to the added line, then a
        // further change. Refreshing recaptures git, rebases the comment onto
        // the new diff, and reloads the review over it.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // The working tree gains a fourth line; this is the diff v0 captures.
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let mut log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        DraftComment {
            author: Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            target: CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(4).unwrap(),
                end_line: LineNo::new(4).unwrap(),
            },
            body: "why delta?".to_string(),
            disposition: None,
        }
        .append(&mut log, LockWait::Block)
        .expect("attach comment");
        drop(log);

        // The working tree gains a line at the top too, so delta slides down and
        // the comment must rebase from line 4 to line 5.
        std::fs::write(&file, "zero\nalpha\nbeta\ngamma\ndelta\n").expect("write v1");

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version.get(),
            state.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        refresh_in_place(
            &session_path,
            &author,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
        )
        .expect("refresh in place");

        // The reloaded review shows the full recaptured v1 diff, with the comment
        // rebased above the added delta on its new line, and the status line
        // reports the tally. A version-comparison prompt is offered over the full
        // diff, opening on the latest where the reviewer was reading.
        wince::assert_eq!(app.picking(), true);
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  f.txt\n",
            "@@ -1,3 +1,5 @@\n",
            "        1 + zero\n",
            "   1    2   alpha\n",
            "   2    3   beta\n",
            "   3    4   gamma\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why delta?                                                                    │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        5 +└delta\n",
            "---\n",
            "captured v1; rebased 1 comment: 1 exact, 0 shifted, 0 moved, 0 outdated\n",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn comparing_against_an_earlier_version_shows_the_change_since_then() {
        // A committed base, a working change captured as v0, then a further
        // change refreshed into v1. Comparing the review against v0 reconstructs
        // the change made between the two versions -- beta becoming BETA -- with
        // the still-present delta as context, rather than the whole v1 diff.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // v0 appends delta to the working tree.
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version.get(),
            state.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        // v1 rewrites beta in the working tree; refreshing captures it.
        std::fs::write(&file, "alpha\nBETA\ngamma\ndelta\n").expect("write v1");
        let author = Author {
            name: "wez".to_string(),
            kind: AuthorKind::Human,
        };
        refresh_in_place(
            &session_path,
            &author,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
        )
        .expect("refresh to v1");

        compare_in_place(
            &session_path,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
            CompareRequest::Version(0),
        )
        .expect("compare");

        // The review now shows only what changed between v0 and v1: beta on the
        // left, BETA on the right, with the unchanged lines as context, and the
        // status line names the comparison.
        let expected = "\
Review [press c here to draft the review comment] [press e to write the description]
modified  f.txt
@@ -1,4 +1,4 @@
   1    1   alpha
   2      - beta
        2 + BETA
   3    3   gamma
   4    4   delta
---
comparing v0 against v1
";
        wince::assert_eq!(screen(&app, 80), expected.to_string());

        // Returning to the latest diff shows v1's own captured change against
        // its baseline again.
        compare_in_place(
            &session_path,
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
            CompareRequest::Latest,
        )
        .expect("back to latest");
        let latest = "\
Review [press c here to draft the review comment] [press e to write the description]
modified  f.txt
@@ -1,3 +1,4 @@
   1    1   alpha
   2      - beta
        2 + BETA
   3    3   gamma
        4 + delta
---
showing the latest diff (v1)
";
        wince::assert_eq!(screen(&app, 80), latest.to_string());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resumed_session_detects_when_its_source_has_moved_on() {
        // A committed base captured as v0. With the working tree untouched since
        // capture, recapturing matches v0 and nothing is offered; changing the
        // working tree makes the recapture differ, which the launch check
        // reports so a resume can prompt to refresh.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let state = ReviewState::load(&session_path).expect("load state");
        wince::assert_eq!(source_changed(&state), false);

        // The working tree gains another line, so a recapture no longer matches
        // v0.
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\nepsilon\n").expect("write change");
        let state = ReviewState::load(&session_path).expect("reload state");
        wince::assert_eq!(source_changed(&state), true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stdin_session_never_offers_a_refresh_on_resume() {
        // A stdin diff cannot be recaptured once the TUI owns the terminal, so
        // resuming such a session never reports its source as changed.
        let data = tempfile::tempdir().expect("data tempdir");
        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(data.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = CapturedDiff {
            text: "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,1 @@
+alpha
"
            .to_string(),
            source: SourceKind::Stdin,
            base_revision: None,
            base_tip_relative: false,
            head_revision: None,
        };
        let log = create_session(data.path(), &identity, data.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let state = ReviewState::load(&session_path).expect("load state");
        wince::assert_eq!(source_changed(&state), false);
    }

    #[test]
    fn saving_commits_the_pending_drafts_and_reloads_them_as_committed() {
        // A session over a one-file diff, with a comment drafted in the TUI but
        // not yet committed. Saving appends it to the log and reloads it as a
        // committed comment, so the review keeps editing without the draft.
        let data = tempfile::tempdir().expect("data tempdir");
        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(data.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = CapturedDiff {
            text: "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+alpha
+beta
+gamma
+delta
"
            .to_string(),
            source: SourceKind::Stdin,
            base_revision: None,
            base_tip_relative: false,
            head_revision: None,
        };
        let log = create_session(data.path(), &identity, data.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("view"),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version.get(),
            state.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        // Land on the first content line (alpha) and draft a comment there.
        app.update(Action::Top);
        for _ in 0..3 {
            app.update(Action::LineDown);
        }
        app.update(Action::AddComment);
        for c in "why alpha?".chars() {
            app.compose_key(KeyPress::new(Key::Char(c)));
        }
        app.compose_key(KeyPress::with_modifiers(Key::Char('d'), true, false, false));

        save_in_place(&session_path, &mut app).expect("save in place");

        // The draft is now a persisted Comment record on the added alpha line.
        let targets: Vec<CommentTarget> = read_records(&session_path)
            .expect("read")
            .into_iter()
            .filter_map(|record| match record.body {
                RecordBody::CommentEvent(CommentEvent {
                    kind: CommentEventKind::Create(create),
                    ..
                }) => Some(create.target),
                _ => None,
            })
            .collect();
        wince::assert_eq!(
            targets,
            vec![CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(1).unwrap(),
                end_line: LineNo::new(1).unwrap(),
            }]
        );

        // The reloaded review shows the comment as committed (no draft badge)
        // above the alpha line, and the status line reports the commit.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "added  f.txt\n",
            "@@ -0,0 +1,4 @@\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why alpha?                                                                    │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        1 +└alpha\n",
            "        2 + beta\n",
            "        3 + gamma\n",
            "        4 + delta\n",
            "---\n",
            "committed 1 change\n",
        );
    }

    #[test]
    fn syncing_picks_up_a_comment_committed_by_another_actor() {
        // A session over a one-file diff, opened with no comments. While it is
        // being reviewed an agent commits a comment on the first added line; the
        // watcher registers the append, the reload folds it in as a committed
        // comment, and the tally reports one added.
        let data = tempfile::tempdir().expect("data tempdir");
        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(data.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = CapturedDiff {
            text: "\
diff --git a/f.txt b/f.txt
new file mode 100644
--- /dev/null
+++ b/f.txt
@@ -0,0 +1,4 @@
+alpha
+beta
+gamma
+delta
"
            .to_string(),
            source: SourceKind::Stdin,
            base_revision: None,
            base_tip_relative: false,
            head_revision: None,
        };
        let log = create_session(data.path(), &identity, data.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let review = Review::new(
            DiffView::new(theme.clone()).expect("view"),
            diff,
            Author {
                name: "wez".to_string(),
                kind: AuthorKind::Human,
            },
            version.get(),
            state.comments.clone(),
            None,
        );
        let mut app = App::reviewing(review, 40, &theme);

        // The watcher takes the freshly created session as its baseline, so it
        // registers no change until another actor writes.
        let mut watcher = SessionWatcher::new(&session_path);
        wince::assert_eq!(watcher.changed().is_some(), false);

        // An agent commits a comment on the alpha line straight to the log.
        let mut log = SessionLog::open(&session_path).expect("open");
        log.append_locked(RecordBody::CommentEvent(CommentEvent {
            id: Ulid(7),
            author: Author {
                name: "assistant".to_string(),
                kind: AuthorKind::Agent,
            },
            authored_at: None,
            origin: None,
            kind: CommentEventKind::Create(CommentCreate {
                target: CommentTarget::Lines {
                    file: "f.txt".to_string(),
                    side: Side::After,
                    start_line: LineNo::new(1).unwrap(),
                    end_line: LineNo::new(1).unwrap(),
                },
                version,
                anchor: None,
                body: "alpha looks off".to_string(),
                disposition: None,
            }),
        }))
        .expect("append comment");

        let fingerprint = watcher.changed().expect("the append registers");
        let summary = reload_committed(&session_path, &mut app).expect("reload");
        watcher.acknowledge(fingerprint);
        app.set_message(sync_report(&summary));

        wince::assert_eq!(
            summary,
            CommentSync {
                added: 1,
                changed: 0,
                description_changed: false,
                removed: 0,
            }
        );
        // The acknowledged change no longer registers.
        wince::assert_eq!(watcher.changed().is_some(), false);

        // The agent's comment now shows as committed above the alpha line, and
        // the status line reports what was synced.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "added  f.txt\n",
            "@@ -0,0 +1,4 @@\n",
            "┌ #1 assistant (agent)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│alpha looks off                                                               │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        1 +└alpha\n",
            "        2 + beta\n",
            "        3 + gamma\n",
            "        4 + delta\n",
            "---\n",
            "synced: 1 added\n",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_comment_drafted_in_the_tui_renders_its_snippet_after_commit() {
        // A reviewer highlights a changed line in the TUI and drafts a comment
        // on it. Committing captures the line's anchor, so `wiff render` shows
        // the fenced snippet the same as a comment added through the CLI.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // The working tree changes the middle line; this is the diff v0 captures.
        std::fs::write(&file, "alpha\nBETA\ngamma\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let mut review = Review::new(
            DiffView::new(theme).expect("renderer"),
            diff,
            wez(),
            version.get(),
            state.comments.clone(),
            None,
        );

        // The reviewer drafts a comment on the changed line, then commits it.
        review.add_comment(
            CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(2).unwrap(),
                end_line: LineNo::new(2).unwrap(),
            },
            "why uppercase?".to_string(),
        );
        let drafts = review.take_drafts();
        commit_drafts(&session_path, drafts).expect("commit");

        // The rendered review shows the comment with its captured snippet: the
        // changed line marked, with the line above and below as context. The
        // session's id is variable, so it is normalized before the comparison.
        let state = ReviewState::load(&session_path).expect("reload state");
        let rendered = crate::render::render(&state, crate::render::Format::Markdown)
            .expect("render markdown");
        let normalized = rendered.replace(&state.session.id.to_string(), "SESSION");
        #[rustfmt::skip]
        wince::snapshot_str!(
            normalized,
            "# Review SESSION\n",
            "\n",
            "- project: demo\n",
            "- source: git working copy\n",
            "- version: v0 (1 file)\n",
            "\n",
            "## Comments\n",
            "\n",
            "### f.txt\n",
            "\n",
            "- #1 line 2 (after) by wez (human)\n",
            "  why uppercase?\n",
            "\n",
            "  ```\n",
            "       1 | alpha\n",
            "  >    2 | BETA\n",
            "       3 | gamma\n",
            "  ```\n",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_draft_refreshed_before_commit_captures_its_anchor_from_the_new_version() {
        // A comment is drafted against v0, then a refresh recaptures the working
        // tree as v1 and rebases the draft forward before it is committed. The
        // commit must capture the anchor from v1, where the draft now lives, so
        // the rendered snippet shows the reviewed line at its v1 position with
        // its v1 context rather than v0's.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        // v0 appends delta; the draft comments on it at line 4.
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let mut review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            wez(),
            version.get(),
            state.comments.clone(),
            None,
        );
        review.add_comment(
            CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(4).unwrap(),
                end_line: LineNo::new(4).unwrap(),
            },
            "why delta?".to_string(),
        );
        let mut app = App::reviewing(review, 40, &theme);

        // A line inserted at the top slides delta from line 4 to line 5; the
        // refresh recaptures this as v1 and rebases the draft onto it.
        std::fs::write(&file, "zero\nalpha\nbeta\ngamma\ndelta\n").expect("write v1");
        refresh_in_place(
            &session_path,
            &wez(),
            wiff_diff::DEFAULT_TAB_WIDTH,
            &mut app,
        )
        .expect("refresh in place");

        let drafts = app.draft_records();
        commit_drafts(&session_path, drafts).expect("commit");

        // The committed comment renders its anchor from v1: delta at line 5 with
        // gamma above it, proving the capture read the version the draft rebased
        // onto rather than the v0 it was authored against.
        let state = ReviewState::load(&session_path).expect("reload state");
        let rendered = crate::render::render(&state, crate::render::Format::Markdown)
            .expect("render markdown");
        let normalized = rendered.replace(&state.session.id.to_string(), "SESSION");
        #[rustfmt::skip]
        wince::snapshot_str!(
            normalized,
            "# Review SESSION\n",
            "\n",
            "- project: demo\n",
            "- source: git working copy\n",
            "- version: v1 (1 file)\n",
            "\n",
            "## Comments\n",
            "\n",
            "### f.txt\n",
            "\n",
            "- #1 line 5 (after) by wez (human)\n",
            "  why delta?\n",
            "\n",
            "  ```\n",
            "       2 | alpha\n",
            "       3 | beta\n",
            "       4 | gamma\n",
            "  >    5 | delta\n",
            "  ```\n",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saving_a_draft_whose_diff_is_gone_reports_the_comment_as_unanchored() {
        // A comment is drafted against v0, then that version's sideband diff is
        // removed before the reviewer saves. The commit cannot read the diff to
        // capture the anchor, so the comment commits as a bare locator and the
        // status line reports it as unanchored without failing the save.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(&file, "alpha\nBETA\ngamma\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let mut review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            wez(),
            version.get(),
            state.comments.clone(),
            None,
        );
        review.add_comment(
            CommentTarget::Lines {
                file: "f.txt".to_string(),
                side: Side::After,
                start_line: LineNo::new(2).unwrap(),
                end_line: LineNo::new(2).unwrap(),
            },
            "why uppercase?".to_string(),
        );
        let mut app = App::reviewing(review, 40, &theme);

        // Remove the sideband diff the anchor would be captured from.
        let diff_path = SessionLog::open(&session_path)
            .unwrap()
            .sideband_dir()
            .join(format!("v{version}.diff"));
        std::fs::remove_file(&diff_path).expect("remove sideband diff");

        save_in_place(&session_path, &mut app).expect("save");

        // The comment commits and shows above the reviewed line; the status line
        // reports the one comment that could not be anchored as a count.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  f.txt\n",
            "@@ -1,3 +1,3 @@\n",
            "   1    1   alpha\n",
            "   2      - beta\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why uppercase?                                                                │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        2 +└BETA\n",
            "   3    3   gamma\n",
            "---\n",
            "committed 1 change; 1 comment could not be anchored\n",
        );

        // The comment persisted as a bare locator, without an anchor.
        let anchors: Vec<Option<wiff_core::record::Anchor>> = ReviewState::load(&session_path)
            .expect("reload state")
            .comments
            .iter()
            .map(|comment| comment.anchor.clone())
            .collect();
        wince::assert_eq!(anchors, vec![None]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saving_two_drafts_whose_diff_is_gone_reports_the_plural_count() {
        // Two comments are drafted against v0, then that version's sideband diff
        // is removed before the reviewer saves. Both commit as bare locators and
        // the status line pluralizes the unanchored count.
        let repo = tempfile::tempdir().expect("repo tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        let file = repo.path().join("f.txt");

        git(repo.path(), &["init", "-q"]);
        std::fs::write(&file, "alpha\nbeta\ngamma\n").expect("write base");
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        std::fs::write(&file, "ALPHA\nBETA\ngamma\n").expect("write v0");

        let identity = ProjectIdentity {
            canonical: "demo".to_string(),
            repo_root: Some(repo.path().to_path_buf()),
            scm: Some(ScmType::Git),
        };
        let captured = capture_scm_diff(
            Some(ScmType::Git),
            repo.path().to_path_buf(),
            DiffSelection::WorkingCopy,
            None,
        )
        .await
        .expect("capture v0");
        let log = create_session(data.path(), &identity, repo.path(), &captured, None)
            .expect("create session");
        let session_path = log.path().to_path_buf();
        drop(log);

        let theme = Theme::dark();
        let state = ReviewState::load(&session_path).expect("load state");
        let version = state.latest_version().expect("a captured version").number;
        let diff = wiff_diff::parse(
            &SessionLog::open(&session_path)
                .unwrap()
                .read_diff(version)
                .unwrap(),
        )
        .expect("parse v0");
        let mut review = Review::new(
            DiffView::new(theme.clone()).expect("renderer"),
            diff,
            wez(),
            version.get(),
            state.comments.clone(),
            None,
        );
        for line in [1u32, 2] {
            review.add_comment(
                CommentTarget::Lines {
                    file: "f.txt".to_string(),
                    side: Side::After,
                    start_line: LineNo::new(line).unwrap(),
                    end_line: LineNo::new(line).unwrap(),
                },
                format!("why line {line}?"),
            );
        }
        let mut app = App::reviewing(review, 40, &theme);

        // Remove the sideband diff both anchors would be captured from.
        let diff_path = SessionLog::open(&session_path)
            .unwrap()
            .sideband_dir()
            .join(format!("v{version}.diff"));
        std::fs::remove_file(&diff_path).expect("remove sideband diff");

        save_in_place(&session_path, &mut app).expect("save");

        // Both comments commit and the status line reports the plural count.
        #[rustfmt::skip]
        wince::snapshot_str!(
            screen(&app, 80),
            "Review [press c here to draft the review comment] [press e to write the description]\n",
            "modified  f.txt\n",
            "@@ -1,3 +1,3 @@\n",
            "   1      - alpha\n",
            "   2      - beta\n",
            "┌ #1 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why line 1?                                                                   │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        1 +└ALPHA\n",
            "┌ #2 wez (human)  press e to edit  r to reply  x to resolve  d to delete  tab to expand/collapse ┐\n",
            "│why line 2?                                                                   │\n",
            "└──────────┬───────────────────────────────────────────────────────────────────┘\n",
            "        2 +└BETA\n",
            "   3    3   gamma\n",
            "---\n",
            "committed 2 changes; 2 comments could not be anchored\n",
        );

        // Both comments persisted as bare locators, without anchors.
        let anchors: Vec<Option<wiff_core::record::Anchor>> = ReviewState::load(&session_path)
            .expect("reload state")
            .comments
            .iter()
            .map(|comment| comment.anchor.clone())
            .collect();
        wince::assert_eq!(anchors, vec![None, None]);
    }
}
