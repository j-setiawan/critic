//! Domain models shared across GitHub, search, and UI layers.

use octocrab::models::{issues, pulls};
use std::fmt;

/// A lightweight pull request summary shown on the search screen.
#[derive(Debug, Clone)]
pub struct PullRequestSummary {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub base_sha: String,
    pub html_url: Option<String>,
    pub updated_at_unix_ms: i64,
    pub created_at_unix_ms: i64,
    pub is_draft: bool,
    pub reviewer_logins: Vec<String>,
    pub review_status: Option<PullRequestReviewStatus>,
}

impl PullRequestSummary {
    /// Returns a searchable composite string used by fuzzy matching.
    pub fn search_text(&self) -> String {
        format!(
            "#{} {} @{} {} -> {}",
            self.number, self.title, self.author, self.head_ref, self.base_ref
        )
    }

    pub fn has_reviewer(&self, login: &str) -> bool {
        self.reviewer_logins
            .iter()
            .any(|reviewer| reviewer.eq_ignore_ascii_case(login))
    }
}

/// Aggregate review state shown on the search list.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PullRequestReviewStatus {
    Approved,
    ChangesRequested,
}

/// A pull request review comment from GitHub API.
pub type ReviewComment = pulls::Comment;

/// An issue-style pull request comment from GitHub API.
pub type IssueComment = issues::Comment;

/// A pull request review summary from GitHub API.
pub type PullReviewSummary = pulls::Review;

/// A hierarchical review thread rooted at a top-level review comment.
#[derive(Debug, Clone)]
pub struct ReviewThread {
    pub thread_id: Option<String>,
    pub is_resolved: bool,
    pub comment: ReviewComment,
    pub replies: Vec<ReviewThread>,
}

/// A merged comment entry shown in the left pane.
#[derive(Debug, Clone)]
pub enum PullRequestComment {
    ReviewThread(Box<ReviewThread>),
    IssueComment(Box<IssueComment>),
    ReviewSummary(Box<PullReviewSummary>),
}

/// All review data required for the review screen.
#[derive(Debug, Clone)]
pub struct PullRequestData {
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub base_sha: String,
    pub changed_files: Vec<String>,
    pub comments: Vec<PullRequestComment>,
    /// Non-fatal warning raised while loading (e.g. review thread resolved
    /// state unavailable). Shown in the header without blocking the review.
    pub load_warning: Option<String>,
}

impl PullRequestData {
    pub fn review_thread_totals(&self) -> (usize, usize) {
        let mut total = 0usize;
        let mut resolved = 0usize;

        for entry in &self.comments {
            if let PullRequestComment::ReviewThread(thread) = entry {
                total += 1;
                if thread.is_resolved {
                    resolved += 1;
                }
            }
        }

        (resolved, total)
    }
}

/// A rendered pull request diff payload for the diff tab.
#[derive(Debug, Clone, Default)]
pub struct PullRequestDiffData {
    pub files: Vec<PullRequestDiffFile>,
}

/// A single changed file in the pull request diff.
#[derive(Debug, Clone)]
pub struct PullRequestDiffFile {
    pub path: String,
    pub status: PullRequestDiffFileStatus,
    pub rows: Vec<PullRequestDiffRow>,
    pub hunk_starts: Vec<usize>,
}

/// File-level status in the pull request diff.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PullRequestDiffFileStatus {
    Modified,
    Added,
    Removed,
}

/// A single aligned diff row.
#[derive(Debug, Clone)]
pub struct PullRequestDiffRow {
    pub left_line_number: Option<usize>,
    pub right_line_number: Option<usize>,
    pub left_text: String,
    pub right_text: String,
    pub left_highlights: Vec<PullRequestDiffHighlightRange>,
    pub right_highlights: Vec<PullRequestDiffHighlightRange>,
    pub kind: PullRequestDiffRowKind,
}

/// Diff row styling category.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PullRequestDiffRowKind {
    Context,
    Added,
    Removed,
    Modified,
}

/// A highlighted character range inside a diff row side.
///
/// `end` is exclusive for normal spans. When `end == FULL_LINE_END`, the
/// range represents a full-line highlight.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PullRequestDiffHighlightRange {
    pub start: usize,
    pub end: usize,
}

impl PullRequestDiffHighlightRange {
    /// Sentinel end value used to represent a full-line highlight region.
    pub const FULL_LINE_END: usize = usize::MAX;

    #[must_use]
    pub const fn full_line() -> Self {
        Self {
            start: 0,
            end: Self::FULL_LINE_END,
        }
    }

    #[must_use]
    pub const fn is_full_line(self) -> bool {
        self.end == Self::FULL_LINE_END
    }
}

/// The current application route.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Route {
    Search,
    Review,
}

/// A flattened left-pane row kind.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ListNodeKind {
    Thread,
    Reply,
    Issue,
    Review,
}

/// A flattened left-pane row for navigation/rendering.
#[derive(Debug, Clone)]
pub struct ListNode {
    pub key: String,
    pub kind: ListNodeKind,
    pub depth: usize,
    pub root_key: Option<String>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comment: CommentRef,
}

/// Union of comment variants attached to a list node.
#[derive(Debug, Clone)]
pub enum CommentRef {
    Review(ReviewComment),
    Issue(IssueComment),
    ReviewSummary(PullReviewSummary),
}

impl CommentRef {
    pub fn author(&self) -> &str {
        match self {
            Self::Review(comment) => comment
                .user
                .as_ref()
                .map(|user| user.login.as_str())
                .unwrap_or("unknown"),
            Self::Issue(comment) => comment.user.login.as_str(),
            Self::ReviewSummary(review) => review
                .user
                .as_ref()
                .map(|user| user.login.as_str())
                .unwrap_or("unknown"),
        }
    }

    pub fn body(&self) -> &str {
        match self {
            Self::Review(comment) => comment.body.as_str(),
            Self::Issue(comment) => comment.body.as_deref().unwrap_or(""),
            Self::ReviewSummary(review) => review.body.as_deref().unwrap_or(""),
        }
    }
}

/// Returns whether a review comment no longer has a usable source location.
pub fn review_comment_is_outdated(comment: &ReviewComment) -> bool {
    let has_path = !comment.path.trim().is_empty();
    let has_line = comment.line.or(comment.start_line).is_some();
    !(has_path && has_line)
}

impl fmt::Display for ListNodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Thread => write!(f, "thread"),
            Self::Reply => write!(f, "reply"),
            Self::Issue => write!(f, "issue"),
            Self::Review => write!(f, "review"),
        }
    }
}
