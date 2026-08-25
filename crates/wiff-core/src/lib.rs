//! The wiff core: the append-only session [`session`] log, its [`record`]
//! schema, and project [`identity`] resolution. This crate owns persistence and
//! discovery; diff parsing and the diff model come from `wiff-diff`.

pub mod base_resolve;
pub mod base_ruleset;
pub mod capture;
pub mod comment;
pub mod compare;
pub mod config;
pub mod description;
pub mod determinism;
pub mod draft;
pub mod error;
pub mod hash;
pub mod identity;
pub mod rebase;
pub mod record;
pub mod refresh;
pub mod review;
pub mod session;
pub mod session_id;
pub mod short_id;
pub mod source;

pub use base_resolve::{RevisionResolver, resolve_base};
pub use base_ruleset::{
    BaseRuleset, DEFAULT_BASE_REVISION_RULES, ParseError, Reference, Rule, RuleOp, Ruleset,
    parse_ruleset,
};
pub use capture::{
    IfNeeded, create_forge_session, create_session, reuse_or_create, write_diff_version,
};
pub use comment::{
    AddedComment, AnchorFailures, DraftComment, capture_draft_anchors, delete_comment,
    edit_comment, set_disposition, set_resolved,
};
pub use compare::{Comparison, LineOrigin, compare_versions};
pub use config::AuthorDefaults;
pub use description::set_description;
pub use draft::{DraftBuffer, EffectiveComment, EffectiveDescription, draft_create};
pub use error::{Error, Result};
pub use hash::SidebandHash;
pub use identity::{ProjectIdentity, ScmType};
pub use rebase::{RebaseOutcome, rebase_line_comment};
pub use refresh::{
    BaseShift, RefreshOutcome, explore_file_set, refresh_session, refresh_session_with,
    widen_explore,
};
pub use review::{CommentState, DescriptionState, ReviewState, fold};
pub use session::{LockWait, ProjectLock, SessionLock, SessionLog};
pub use session_id::SessionId;
pub use short_id::ShortId;
pub use source::{
    CapturedDiff, DiffSource, ExploreCapture, FetchSource, GitSource, JjRepo, JjSource, Remote,
    ScmRepo, SkipReason, TrackingBranch, capture_explore,
};
