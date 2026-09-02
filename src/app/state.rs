//! Application state models and route-local behavior.

mod diff_tree;
mod search_input;
mod thread_nodes;
mod thread_search;
mod tree_filter;

pub use self::search_input::SearchInputState;
use self::{
    diff_tree::{build_diff_tree_rows, filter_diff_tree_rows},
    thread_nodes::{append_thread_nodes, is_review_group_key, review_group_key, thread_key},
    thread_search::filter_thread_nodes,
};
use crate::{
    domain::{
        CommentRef, ListNode, ListNodeKind, PullRequestComment, PullRequestData,
        PullRequestDiffData, PullRequestDiffFile, PullRequestSummary, ReviewThread, Route,
    },
    search::fuzzy::rank_pull_requests,
};
use std::collections::{HashMap, HashSet};

/// Spinner frames used for active async operations.
pub const SPINNER_FRAMES: [&str; 8] = ["⢎⡰", "⢎⡡", "⢎⡑", "⢎⠱", "⠎⡱", "⢊⡱", "⢌⡱", "⢆⡱"];

/// Top-level mutable application state.
#[derive(Debug)]
pub struct AppState {
    pub route: Route,
    pub should_quit: bool,
    pub error_message: Option<String>,
    pub repository_label: String,
    pub pull_requests: Vec<PullRequestSummary>,
    pub search_input: SearchInputState,
    pub search_results: Vec<usize>,
    pub search_selected: usize,
    pub viewer_login: Option<String>,
    pub search_scope: SearchScope,
    pub search_status_filter: SearchStatusFilter,
    pub search_sort: SearchSort,
    pub review: Option<ReviewScreenState>,
    operation: Option<OperationState>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            route: Route::Search,
            should_quit: false,
            error_message: None,
            repository_label: "(resolving repository)".to_owned(),
            pull_requests: Vec::new(),
            search_input: SearchInputState::default(),
            search_results: Vec::new(),
            search_selected: 0,
            viewer_login: None,
            search_scope: SearchScope::All,
            search_status_filter: SearchStatusFilter::All,
            search_sort: SearchSort::UpdatedAt,
            review: None,
            operation: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SearchSort {
    UpdatedAt,
    CreatedAt,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SearchScope {
    All,
    Author,
    Reviewer,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SearchStatusFilter {
    All,
    Draft,
    Ready,
    Approved,
    Rejected,
}

impl SearchStatusFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Draft => "draft",
            Self::Ready => "ready",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

impl SearchScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Author => "author",
            Self::Reviewer => "reviewer",
        }
    }
}

impl SearchSort {
    pub fn label(self) -> &'static str {
        match self {
            Self::UpdatedAt => "updated",
            Self::CreatedAt => "created",
        }
    }
}

impl AppState {
    pub fn set_repository_label(&mut self, label: String) {
        self.repository_label = label;
    }

    pub fn set_pull_requests(&mut self, pulls: Vec<PullRequestSummary>) {
        self.pull_requests = pulls;
        self.recompute_search();
        self.search_selected = 0;
    }

    pub fn set_viewer_login(&mut self, login: Option<String>) {
        self.viewer_login = login;
        self.recompute_search();
    }

    pub fn recompute_search(&mut self) {
        let viewer = self
            .viewer_login
            .as_ref()
            .map(|value| value.to_ascii_lowercase());

        let query = self.search_input.query();
        let has_query = !query.trim().is_empty();

        self.search_results = rank_pull_requests(query, &self.pull_requests)
            .into_iter()
            .filter(|result| {
                self.pull_requests.get(result.index).is_some_and(|pull| {
                    scope_matches(pull, self.search_scope, viewer.as_deref())
                        && status_matches(pull, self.search_status_filter)
                })
            })
            .map(|result| result.index)
            .collect();

        if !has_query {
            self.search_results.sort_by(|a, b| {
                let a_pull = self.pull_requests.get(*a);
                let b_pull = self.pull_requests.get(*b);
                let a_ts = match self.search_sort {
                    SearchSort::UpdatedAt => a_pull.map(|pull| pull.updated_at_unix_ms),
                    SearchSort::CreatedAt => a_pull.map(|pull| pull.created_at_unix_ms),
                }
                .unwrap_or_default();
                let b_ts = match self.search_sort {
                    SearchSort::UpdatedAt => b_pull.map(|pull| pull.updated_at_unix_ms),
                    SearchSort::CreatedAt => b_pull.map(|pull| pull.created_at_unix_ms),
                }
                .unwrap_or_default();
                b_ts.cmp(&a_ts)
            });
        }

        if self.search_selected >= self.search_results.len() {
            self.search_selected = self.search_results.len().saturating_sub(1);
        }
    }

    pub fn toggle_search_scope(&mut self) {
        self.search_scope = match self.search_scope {
            SearchScope::All => SearchScope::Author,
            SearchScope::Author => SearchScope::Reviewer,
            SearchScope::Reviewer => SearchScope::All,
        };
        self.recompute_search();
    }

    pub fn toggle_search_status_filter(&mut self) {
        self.search_status_filter = match self.search_status_filter {
            SearchStatusFilter::All => SearchStatusFilter::Draft,
            SearchStatusFilter::Draft => SearchStatusFilter::Ready,
            SearchStatusFilter::Ready => SearchStatusFilter::Approved,
            SearchStatusFilter::Approved => SearchStatusFilter::Rejected,
            SearchStatusFilter::Rejected => SearchStatusFilter::All,
        };
        self.recompute_search();
    }

    pub fn toggle_search_sort(&mut self) {
        self.search_sort = match self.search_sort {
            SearchSort::UpdatedAt => SearchSort::CreatedAt,
            SearchSort::CreatedAt => SearchSort::UpdatedAt,
        };
        self.recompute_search();
    }

    pub fn selected_search_pull(&self) -> Option<&PullRequestSummary> {
        let index = *self.search_results.get(self.search_selected)?;
        self.pull_requests.get(index)
    }

    pub fn focus_search(&mut self) {
        self.search_input.focus();
    }

    pub fn is_search_focused(&self) -> bool {
        self.search_input.is_focused()
    }

    pub fn search_query(&self) -> &str {
        self.search_input.query()
    }

    pub fn search_input_mut(&mut self) -> &mut SearchInputState {
        &mut self.search_input
    }

    pub fn search_move_down(&mut self) {
        if self.search_results.is_empty() {
            self.search_selected = 0;
            return;
        }

        self.search_selected = (self.search_selected + 1).min(self.search_results.len() - 1);
    }

    pub fn search_move_up(&mut self) {
        if self.search_results.is_empty() {
            self.search_selected = 0;
            return;
        }

        self.search_selected = self.search_selected.saturating_sub(1);
    }

    pub fn open_review(&mut self, pull: PullRequestSummary, data: PullRequestData) {
        self.review = Some(ReviewScreenState::new(pull, data));
        self.route = Route::Review;
    }

    pub fn back_to_search(&mut self) {
        self.route = Route::Search;
        self.search_input.unfocus();
    }

    pub fn begin_operation(&mut self, label: impl Into<String>) {
        self.operation = Some(OperationState {
            label: label.into(),
            spinner_index: 0,
        });
    }

    pub fn end_operation(&mut self) {
        self.operation = None;
    }

    pub fn is_busy(&self) -> bool {
        self.operation.is_some()
    }

    pub fn advance_spinner(&mut self) {
        if let Some(operation) = self.operation.as_mut() {
            operation.spinner_index = (operation.spinner_index + 1) % SPINNER_FRAMES.len();
        }
    }

    pub fn operation_display(&self) -> Option<String> {
        let operation = self.operation.as_ref()?;
        let frame = SPINNER_FRAMES
            .get(operation.spinner_index)
            .copied()
            .unwrap_or("⢎⡰");
        Some(format!("{frame} {}", operation.label))
    }
}

fn scope_matches(
    pull: &PullRequestSummary,
    scope: SearchScope,
    viewer_login: Option<&str>,
) -> bool {
    match scope {
        SearchScope::All => true,
        SearchScope::Author => viewer_login
            .map(|login| pull.author.eq_ignore_ascii_case(login))
            .unwrap_or(false),
        SearchScope::Reviewer => viewer_login
            .map(|login| pull.has_reviewer(login))
            .unwrap_or(false),
    }
}

fn status_matches(pull: &PullRequestSummary, status: SearchStatusFilter) -> bool {
    use crate::domain::PullRequestReviewStatus;

    match status {
        SearchStatusFilter::All => true,
        SearchStatusFilter::Draft => pull.is_draft,
        SearchStatusFilter::Ready => !pull.is_draft,
        SearchStatusFilter::Approved => {
            matches!(pull.review_status, Some(PullRequestReviewStatus::Approved))
        }
        SearchStatusFilter::Rejected => {
            matches!(
                pull.review_status,
                Some(PullRequestReviewStatus::ChangesRequested)
            )
        }
    }
}

#[derive(Debug, Clone)]
struct OperationState {
    label: String,
    spinner_index: usize,
}

/// Review submission event kind.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ReviewSubmissionEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewSubmissionEvent {
    pub fn as_api_event(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Comment => "Submit Review Comment",
            Self::Approve => "Submit Approval",
            Self::RequestChanges => "Request Changes",
        }
    }
}

/// Thread context resolved from the current selection.
#[derive(Debug, Clone)]
pub struct ThreadActionContext {
    pub root_key: String,
    pub thread_id: Option<String>,
    pub comment_id: u64,
    pub is_resolved: bool,
}

/// Route-local state for the review screen.
#[derive(Debug, Clone)]
pub struct ReviewScreenState {
    pub pull: PullRequestSummary,
    pub data: PullRequestData,
    pub active_tab: ReviewTab,
    pub hide_resolved: bool,
    pub selected_row: usize,
    pub right_scroll: u16,
    pub diff: Option<PullRequestDiffData>,
    pub diff_error: Option<String>,
    pub diff_focus: DiffFocus,
    pub selected_diff_row: usize,
    pub selected_diff_file: usize,
    pub selected_diff_line: usize,
    pub diff_selection_anchor: Option<usize>,
    pub diff_scroll: u16,
    pending_preview_scroll: u16,
    pending_preview_comment_id: Option<u64>,
    pub selected_hunk: usize,
    pub diff_viewport_height: u16,
    pub thread_search: SearchInputState,
    pub diff_search: SearchInputState,
    pub diff_tree_rows_cache: Vec<DiffTreeRow>,
    pub pending_review_comments: Vec<PendingReviewCommentDraft>,
    thread_nodes_cache: Vec<ListNode>,
    pub nodes: Vec<ListNode>,
    pub reply_drafts: HashMap<String, String>,
    next_pending_review_comment_id: u64,
    collapsed: HashSet<String>,
    diff_collapsed_dirs: HashSet<String>,
    threads_by_key: HashMap<String, ReviewThread>,
}

impl ReviewScreenState {
    pub fn new(pull: PullRequestSummary, data: PullRequestData) -> Self {
        let mut pull = pull;
        pull.head_ref = data.head_ref.clone();
        pull.base_ref = data.base_ref.clone();
        pull.head_sha = data.head_sha.clone();
        pull.base_sha = data.base_sha.clone();

        let mut state = Self {
            pull,
            data,
            active_tab: ReviewTab::Threads,
            hide_resolved: true,
            selected_row: 0,
            right_scroll: 0,
            diff: None,
            diff_error: None,
            diff_focus: DiffFocus::Files,
            selected_diff_row: 0,
            selected_diff_file: 0,
            selected_diff_line: 0,
            diff_selection_anchor: None,
            diff_scroll: 0,
            pending_preview_scroll: 0,
            pending_preview_comment_id: None,
            selected_hunk: 0,
            diff_viewport_height: 0,
            thread_search: SearchInputState::default(),
            diff_search: SearchInputState::default(),
            diff_tree_rows_cache: Vec::new(),
            pending_review_comments: Vec::new(),
            thread_nodes_cache: Vec::new(),
            nodes: Vec::new(),
            reply_drafts: HashMap::new(),
            next_pending_review_comment_id: 1,
            collapsed: HashSet::new(),
            diff_collapsed_dirs: HashSet::new(),
            threads_by_key: HashMap::new(),
        };

        state.initialize_collapsed_defaults();
        state.rebuild_nodes();
        state
    }

    fn initialize_collapsed_defaults(&mut self) {
        for entry in &self.data.comments {
            if let PullRequestComment::ReviewThread(thread) = entry
                && thread.is_resolved
            {
                self.collapsed.insert(thread_key(thread));
            }
        }
    }

    pub fn rebuild_nodes(&mut self) {
        let selected_key = self.selected_node().map(|node| node.key.clone());
        self.thread_nodes_cache.clear();
        self.threads_by_key.clear();

        let summary_review_ids: HashSet<u64> = self
            .data
            .comments
            .iter()
            .filter_map(|entry| match entry {
                PullRequestComment::ReviewSummary(review) => Some(review.id.into_inner()),
                _ => None,
            })
            .collect();
        let mut grouped_threads: HashMap<u64, Vec<ReviewThread>> = HashMap::new();

        for entry in &self.data.comments {
            let PullRequestComment::ReviewThread(thread) = entry else {
                continue;
            };
            let Some(review_id) = thread
                .comment
                .pull_request_review_id
                .map(|id| id.into_inner())
            else {
                continue;
            };
            if !summary_review_ids.contains(&review_id) {
                continue;
            }
            grouped_threads
                .entry(review_id)
                .or_default()
                .push((**thread).clone());
        }

        for entry in &self.data.comments {
            match entry {
                PullRequestComment::ReviewThread(thread) => {
                    let review_id = thread
                        .comment
                        .pull_request_review_id
                        .map(|id| id.into_inner());
                    if review_id.is_some_and(|id| summary_review_ids.contains(&id)) {
                        continue;
                    }
                    if self.hide_resolved && thread.is_resolved {
                        continue;
                    }
                    append_thread_nodes(
                        &mut self.thread_nodes_cache,
                        &mut self.threads_by_key,
                        &self.collapsed,
                        thread,
                        0,
                    );
                }
                PullRequestComment::IssueComment(comment) => {
                    self.thread_nodes_cache.push(ListNode {
                        key: format!("issue:{}", comment.id),
                        kind: ListNodeKind::Issue,
                        depth: 0,
                        root_key: None,
                        is_resolved: false,
                        is_outdated: false,
                        comment: CommentRef::Issue((**comment).clone()),
                    });
                }
                PullRequestComment::ReviewSummary(review) => {
                    let review_id = review.id.into_inner();
                    let summary_threads = grouped_threads.remove(&review_id).unwrap_or_default();
                    if summary_threads.is_empty() {
                        self.thread_nodes_cache.push(ListNode {
                            key: format!("review-issue:{}", review.id),
                            kind: ListNodeKind::Issue,
                            depth: 0,
                            root_key: None,
                            is_resolved: false,
                            is_outdated: false,
                            comment: CommentRef::ReviewSummary((**review).clone()),
                        });
                        continue;
                    }

                    let has_unresolved_threads =
                        summary_threads.iter().any(|thread| !thread.is_resolved);
                    let group_resolved = !has_unresolved_threads;
                    if self.hide_resolved && group_resolved {
                        continue;
                    }

                    let group_key = review_group_key(review_id);
                    self.thread_nodes_cache.push(ListNode {
                        key: group_key.clone(),
                        kind: ListNodeKind::Review,
                        depth: 0,
                        root_key: None,
                        is_resolved: group_resolved,
                        is_outdated: false,
                        comment: CommentRef::ReviewSummary((**review).clone()),
                    });

                    if self.collapsed.contains(&group_key) {
                        continue;
                    }

                    for thread in &summary_threads {
                        if self.hide_resolved && thread.is_resolved {
                            continue;
                        }
                        append_thread_nodes(
                            &mut self.thread_nodes_cache,
                            &mut self.threads_by_key,
                            &self.collapsed,
                            thread,
                            1,
                        );
                    }
                }
            }
        }

        self.recompute_thread_nodes_cache(selected_key.as_deref());
    }

    fn recompute_thread_nodes_cache(&mut self, selected_key: Option<&str>) {
        self.nodes = filter_thread_nodes(&self.thread_nodes_cache, self.thread_search.query());

        if let Some(previous_key) = selected_key {
            self.selected_row = self
                .nodes
                .iter()
                .position(|node| node.key == previous_key)
                .unwrap_or(0);
        } else {
            self.selected_row = 0;
        }

        if self.selected_row >= self.nodes.len() {
            self.selected_row = self.nodes.len().saturating_sub(1);
        }

        self.right_scroll = 0;
    }

    pub fn selected_node(&self) -> Option<&ListNode> {
        self.nodes.get(self.selected_row)
    }

    pub fn selected_root_thread(&self) -> Option<&ReviewThread> {
        let node = self.selected_node()?;
        let key = node.root_key.as_ref()?;
        self.threads_by_key.get(key)
    }

    pub fn selected_thread_context(&self) -> Option<ThreadActionContext> {
        let node = self.selected_node()?;
        let root_key = node.root_key.as_ref()?.to_owned();
        let thread = self.threads_by_key.get(&root_key)?;

        Some(ThreadActionContext {
            root_key,
            thread_id: thread.thread_id.clone(),
            comment_id: thread.comment.id.into_inner(),
            is_resolved: thread.is_resolved,
        })
    }

    pub fn selected_reply_draft(&self) -> Option<&str> {
        let context = self.selected_thread_context()?;
        self.reply_drafts.get(&context.root_key).map(String::as_str)
    }

    pub fn set_reply_draft(&mut self, root_key: String, body: String) {
        self.reply_drafts.insert(root_key, body);
    }

    pub fn clear_reply_draft(&mut self, root_key: &str) {
        self.reply_drafts.remove(root_key);
    }

    pub fn focus_thread_search(&mut self) {
        self.thread_search.focus();
    }

    pub fn is_thread_search_focused(&self) -> bool {
        self.thread_search.is_focused()
    }

    pub fn thread_search_query(&self) -> &str {
        self.thread_search.query()
    }

    pub fn thread_search_input_mut(&mut self) -> &mut SearchInputState {
        &mut self.thread_search
    }

    pub fn refresh_thread_search_results(&mut self) {
        let selected_key = self.selected_node().map(|node| node.key.clone());
        self.recompute_thread_nodes_cache(selected_key.as_deref());
    }

    pub fn thread_node_count(&self) -> usize {
        self.thread_nodes_cache.len()
    }

    pub fn move_down(&mut self) {
        match self.active_tab {
            ReviewTab::Threads => {
                if self.nodes.is_empty() {
                    self.selected_row = 0;
                    return;
                }

                self.selected_row = (self.selected_row + 1).min(self.nodes.len() - 1);
                self.right_scroll = 0;
            }
            ReviewTab::Diff => {
                if self.diff_focus == DiffFocus::Content {
                    self.move_selected_diff_line_down();
                } else {
                    self.move_diff_tree_selection(NavDirection::Down);
                }
            }
        }
    }

    pub fn move_up(&mut self) {
        match self.active_tab {
            ReviewTab::Threads => {
                if self.nodes.is_empty() {
                    self.selected_row = 0;
                    return;
                }

                self.selected_row = self.selected_row.saturating_sub(1);
                self.right_scroll = 0;
            }
            ReviewTab::Diff => {
                if self.diff_focus == DiffFocus::Content {
                    self.move_selected_diff_line_up();
                } else {
                    self.move_diff_tree_selection(NavDirection::Up);
                }
            }
        }
    }

    pub fn scroll_preview_down(&mut self) {
        match self.active_tab {
            ReviewTab::Threads => {
                self.right_scroll = self.right_scroll.saturating_add(1);
            }
            ReviewTab::Diff => {
                self.sync_pending_review_preview_target();
                if self.pending_preview_comment_id.is_some() {
                    self.pending_preview_scroll = self.pending_preview_scroll.saturating_add(1);
                } else {
                    self.diff_scroll = self.diff_scroll.saturating_add(1);
                }
            }
        }
    }

    pub fn scroll_preview_up(&mut self) {
        match self.active_tab {
            ReviewTab::Threads => {
                self.right_scroll = self.right_scroll.saturating_sub(1);
            }
            ReviewTab::Diff => {
                self.sync_pending_review_preview_target();
                if self.pending_preview_comment_id.is_some() {
                    self.pending_preview_scroll = self.pending_preview_scroll.saturating_sub(1);
                } else {
                    self.diff_scroll = self.diff_scroll.saturating_sub(1);
                }
            }
        }
    }

    pub fn toggle_selected_thread_collapsed(&mut self) {
        let Some(node) = self.selected_node() else {
            return;
        };

        let is_collapsible_review_group =
            node.kind == ListNodeKind::Review && is_review_group_key(&node.key);
        if node.kind != ListNodeKind::Thread && !is_collapsible_review_group {
            return;
        }
        let key = node.key.clone();

        if self.collapsed.contains(&key) {
            self.collapsed.remove(&key);
        } else {
            self.collapsed.insert(key);
        }

        self.rebuild_nodes();
    }

    pub fn toggle_resolved_filter(&mut self) {
        self.hide_resolved = !self.hide_resolved;
        self.rebuild_nodes();
    }

    /// Replaces PR payload data while preserving route-local interaction state.
    ///
    /// Returns `true` when the pull request head SHA changed.
    pub fn set_data(&mut self, data: PullRequestData) -> bool {
        let head_changed = self.pull.head_sha != data.head_sha;

        self.pull.head_ref = data.head_ref.clone();
        self.pull.base_ref = data.base_ref.clone();
        self.pull.head_sha = data.head_sha.clone();
        self.pull.base_sha = data.base_sha.clone();
        self.data = data;
        self.rebuild_nodes();
        head_changed
    }

    /// Clears all loaded diff data and resets diff-focused interaction state.
    pub fn clear_diff(&mut self) {
        self.diff = None;
        self.diff_error = None;
        self.reset_diff_view_state();
    }

    /// Sets the loaded diff payload and transitions to a fresh diff browsing state.
    pub fn set_diff(&mut self, diff: PullRequestDiffData) {
        self.diff = Some(diff);
        self.diff_error = None;
        self.reset_diff_view_state();
        self.recompute_diff_tree_rows_cache();
        self.select_initial_diff_file_row();
    }

    /// Stores a diff loading error and resets diff browsing selections.
    pub fn set_diff_error(&mut self, error: String) {
        self.diff = None;
        self.diff_error = Some(error);
        self.reset_diff_view_state();
    }

    fn reset_diff_view_state(&mut self) {
        self.diff_focus = DiffFocus::Files;
        self.selected_diff_row = 0;
        self.selected_diff_file = 0;
        self.selected_diff_line = 0;
        self.diff_selection_anchor = None;
        self.diff_scroll = 0;
        self.pending_preview_scroll = 0;
        self.pending_preview_comment_id = None;
        self.selected_hunk = 0;
        self.diff_viewport_height = 0;
        self.diff_collapsed_dirs.clear();
        self.diff_search = SearchInputState::default();
        self.diff_tree_rows_cache.clear();
    }

    pub fn active_tab(&self) -> ReviewTab {
        self.active_tab
    }

    pub fn next_tab(&mut self) {
        self.active_tab = match self.active_tab {
            ReviewTab::Threads => ReviewTab::Diff,
            ReviewTab::Diff => ReviewTab::Threads,
        };
    }

    pub fn selected_diff_file(&self) -> Option<&PullRequestDiffFile> {
        let diff = self.diff.as_ref()?;
        diff.files.get(self.selected_diff_file)
    }

    pub fn diff_file_count(&self) -> usize {
        self.diff
            .as_ref()
            .map(|value| value.files.len())
            .unwrap_or(0)
    }

    pub fn diff_tree_rows(&self) -> &[DiffTreeRow] {
        &self.diff_tree_rows_cache
    }

    pub fn selected_diff_tree_row(&self) -> usize {
        self.selected_diff_row
    }

    pub fn toggle_selected_diff_directory_collapsed(&mut self) {
        let Some((row_key, is_directory)) = ({
            let rows = self.diff_tree_rows();
            rows.get(self.selected_diff_row)
                .map(|row| (row.key.clone(), row.is_directory))
        }) else {
            return;
        };
        if !is_directory {
            return;
        }

        if self.diff_collapsed_dirs.contains(&row_key) {
            self.diff_collapsed_dirs.remove(&row_key);
        } else {
            self.diff_collapsed_dirs.insert(row_key.clone());
        }
        self.recompute_diff_tree_rows_cache();

        let Some((row_index, file_index)) = ({
            let rows = self.diff_tree_rows();
            if rows.is_empty() {
                None
            } else {
                let row_index = rows
                    .iter()
                    .position(|candidate| candidate.key == row_key)
                    .unwrap_or_else(|| self.selected_diff_row.min(rows.len().saturating_sub(1)));
                Some((
                    row_index,
                    rows.get(row_index).and_then(|row| row.file_index),
                ))
            }
        }) else {
            self.selected_diff_row = 0;
            self.selected_diff_file = 0;
            self.diff_scroll = 0;
            self.selected_hunk = 0;
            return;
        };

        self.selected_diff_row = row_index;
        if let Some(file_index) = file_index {
            self.set_selected_diff_file(file_index);
        } else {
            self.selected_diff_file = 0;
            self.diff_scroll = 0;
            self.selected_hunk = 0;
        }
    }

    pub fn focus_diff_search(&mut self) {
        self.diff_search.focus();
    }

    pub fn is_diff_search_focused(&self) -> bool {
        self.diff_search.is_focused()
    }

    pub fn focus_diff_files(&mut self) {
        self.diff_focus = DiffFocus::Files;
    }

    pub fn focus_diff_content(&mut self) {
        self.diff_focus = DiffFocus::Content;
    }

    pub fn is_diff_content_focused(&self) -> bool {
        self.diff_focus == DiffFocus::Content
    }

    pub fn diff_search_query(&self) -> &str {
        self.diff_search.query()
    }

    pub fn diff_search_input_mut(&mut self) -> &mut SearchInputState {
        &mut self.diff_search
    }

    pub fn refresh_diff_search_results(&mut self) {
        self.recompute_diff_tree_rows_cache();
        self.realign_diff_selection_for_filter();
    }

    pub fn jump_next_hunk(&mut self) {
        let Some(diff) = self.diff.as_ref() else {
            return;
        };
        let files = self.navigable_diff_files();
        if files.is_empty() {
            return;
        }

        let current_file = self.selected_diff_file;
        let current = self.selected_diff_line;
        let Some(current_pos) = files.iter().position(|index| *index == current_file) else {
            return;
        };

        if let Some(file) = diff.files.get(current_file)
            && let Some(next_hunk) = file
                .hunk_starts
                .iter()
                .copied()
                .find(|start| *start > current)
        {
            let hunk_index = file
                .hunk_starts
                .iter()
                .position(|start| *start == next_hunk)
                .unwrap_or(0);
            self.set_active_hunk(current_file, next_hunk, hunk_index);
            return;
        }

        for offset in 1..files.len() {
            let file_index = files[(current_pos + offset) % files.len()];
            let Some(file) = diff.files.get(file_index) else {
                continue;
            };
            if let Some(next_hunk) = file.hunk_starts.first().copied() {
                self.set_active_hunk(file_index, next_hunk, 0);
                return;
            }
        }

        if let Some(file) = diff.files.get(current_file)
            && let Some(next_hunk) = file.hunk_starts.first().copied()
        {
            self.set_active_hunk(current_file, next_hunk, 0);
        }
    }

    pub fn jump_prev_hunk(&mut self) {
        let Some(diff) = self.diff.as_ref() else {
            return;
        };
        let files = self.navigable_diff_files();
        if files.is_empty() {
            return;
        }

        let current_file = self.selected_diff_file;
        let current = self.selected_diff_line;
        let Some(current_pos) = files.iter().position(|index| *index == current_file) else {
            return;
        };

        if let Some(file) = diff.files.get(current_file)
            && let Some(previous_hunk) = file
                .hunk_starts
                .iter()
                .copied()
                .rev()
                .find(|start| *start < current)
        {
            let hunk_index = file
                .hunk_starts
                .iter()
                .position(|start| *start == previous_hunk)
                .unwrap_or(0);
            self.set_active_hunk(current_file, previous_hunk, hunk_index);
            return;
        }

        for offset in 1..files.len() {
            let file_index = files[(current_pos + files.len() - offset) % files.len()];
            let Some(file) = diff.files.get(file_index) else {
                continue;
            };
            if let Some(previous_hunk) = file.hunk_starts.last().copied() {
                let hunk_index = file.hunk_starts.len().saturating_sub(1);
                self.set_active_hunk(file_index, previous_hunk, hunk_index);
                return;
            }
        }

        if let Some(file) = diff.files.get(current_file)
            && let Some(previous_hunk) = file.hunk_starts.last().copied()
        {
            let hunk_index = file.hunk_starts.len().saturating_sub(1);
            self.set_active_hunk(current_file, previous_hunk, hunk_index);
        }
    }

    pub fn jump_next_pending_review_comment(&mut self) -> bool {
        let locations = self.pending_comment_locations();
        if locations.is_empty() {
            return false;
        }

        let current = (self.selected_diff_file, self.selected_diff_line);
        let target = locations
            .iter()
            .copied()
            .find(|location| (location.file_index, location.row_index) > current)
            .unwrap_or(locations[0]);
        self.jump_to_pending_comment_location(target);
        true
    }

    pub fn jump_prev_pending_review_comment(&mut self) -> bool {
        let locations = self.pending_comment_locations();
        if locations.is_empty() {
            return false;
        }

        let current = (self.selected_diff_file, self.selected_diff_line);
        let target = locations
            .iter()
            .rev()
            .copied()
            .find(|location| (location.file_index, location.row_index) < current)
            .unwrap_or(*locations.last().expect("locations is not empty"));
        self.jump_to_pending_comment_location(target);
        true
    }

    pub fn is_collapsed(&self, key: &str) -> bool {
        self.collapsed.contains(key)
    }

    pub fn selected_diff_line(&self) -> usize {
        self.selected_diff_line
    }

    pub fn selected_diff_range(&self) -> Option<(usize, usize)> {
        let anchor = self.diff_selection_anchor?;
        let start = anchor.min(self.selected_diff_line);
        let end = anchor.max(self.selected_diff_line);
        Some((start, end))
    }

    pub fn toggle_diff_selection_anchor(&mut self) {
        self.diff_selection_anchor = match self.diff_selection_anchor {
            Some(_) => None,
            None => {
                let Some(file) = self.selected_diff_file() else {
                    return;
                };
                self.selected_diff_comment_side().and_then(|side| {
                    if self.row_overlaps_pending_comment(self.selected_diff_line, side) {
                        return None;
                    }

                    hunk_index_for_row(file, self.selected_diff_line)
                        .map(|_| self.selected_diff_line)
                })
            }
        };
    }

    pub fn clear_diff_selection_anchor(&mut self) {
        self.diff_selection_anchor = None;
    }

    pub fn has_diff_selection_anchor(&self) -> bool {
        self.diff_selection_anchor.is_some()
    }

    pub fn pending_review_comment_count(&self) -> usize {
        self.pending_review_comments.len()
    }

    pub fn pending_review_comments(&self) -> &[PendingReviewCommentDraft] {
        &self.pending_review_comments
    }

    pub fn clear_pending_review_comments(&mut self) {
        self.pending_review_comments.clear();
        self.next_pending_review_comment_id = 1;
        self.diff_selection_anchor = None;
    }

    pub fn apply_restored_drafts(
        &mut self,
        mut pending: Vec<PendingReviewCommentDraft>,
        mut reply_drafts: HashMap<String, String>,
    ) -> (usize, usize) {
        pending.retain(|comment| !comment.body.trim().is_empty());
        let changed_files = self
            .data
            .changed_files
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        pending.retain(|comment| changed_files.contains(comment.path.as_str()));

        let valid_roots = self
            .data
            .comments
            .iter()
            .filter_map(|entry| match entry {
                PullRequestComment::ReviewThread(thread) => Some(thread_key(thread)),
                _ => None,
            })
            .collect::<HashSet<_>>();
        reply_drafts.retain(|root, body| valid_roots.contains(root) && !body.trim().is_empty());

        self.next_pending_review_comment_id = pending
            .iter()
            .map(|comment| comment.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.pending_review_comments = pending;
        self.reply_drafts = reply_drafts;

        (
            self.pending_review_comments.len(),
            self.reply_drafts
                .values()
                .filter(|body| !body.is_empty())
                .count(),
        )
    }

    pub fn pending_review_comment_is_outdated(&self, comment: &PendingReviewCommentDraft) -> bool {
        let Some(diff) = self.diff.as_ref() else {
            return false;
        };
        !pending_comment_matches_current_diff(diff, comment)
    }

    pub fn pending_review_comments_for_file(
        &self,
        file: &PullRequestDiffFile,
    ) -> Vec<&PendingReviewCommentDraft> {
        self.pending_review_comments
            .iter()
            .filter(|comment| comment.path == file.path)
            .collect()
    }

    pub fn selected_pending_review_comment(&self) -> Option<&PendingReviewCommentDraft> {
        let file = self.selected_diff_file()?;
        let row = file.rows.get(self.selected_diff_line)?;
        self.pending_review_comments.iter().find(|comment| {
            if comment.path != file.path {
                return false;
            }
            let Some(line) = row_line_for_side(row, comment.side) else {
                return false;
            };
            pending_comment_contains_line(comment, line as u64)
        })
    }

    pub fn sync_pending_review_preview_target(&mut self) {
        let hovered_comment_id = self
            .selected_pending_review_comment()
            .map(|comment| comment.id);
        if hovered_comment_id != self.pending_preview_comment_id {
            self.pending_preview_comment_id = hovered_comment_id;
            self.pending_preview_scroll = 0;
        }
    }

    pub fn clamp_pending_preview_scroll(&mut self, max_scroll: usize) -> usize {
        self.sync_pending_review_preview_target();
        let scroll = usize::from(self.pending_preview_scroll).min(max_scroll);
        self.pending_preview_scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
        scroll
    }

    pub fn remove_selected_pending_review_comment(&mut self) -> bool {
        let Some(comment_id) = self
            .selected_pending_review_comment()
            .map(|comment| comment.id)
        else {
            return false;
        };

        if let Some(index) = self
            .pending_review_comments
            .iter()
            .position(|comment| comment.id == comment_id)
        {
            self.pending_review_comments.remove(index);
            self.diff_selection_anchor = None;
            return true;
        }

        false
    }

    pub fn upsert_pending_review_comment_from_selection(
        &mut self,
        body: String,
    ) -> Result<(), &'static str> {
        let body = body.trim().to_owned();
        if body.is_empty() {
            return Err("comment is empty");
        }

        let Some(file) = self.selected_diff_file() else {
            return Err("no diff file selected");
        };
        let Some(side) = self.selected_diff_comment_side() else {
            return Err("selected line is outside changed diff lines");
        };
        if !self.selection_is_commentable_for_side(side) {
            return Err("selection must stay within changed lines in a single hunk");
        };

        // When the cursor is inside an existing pending comment range, [e] should
        // edit that comment even without selecting the full original range.
        if let Some(existing_id) = self
            .selected_pending_review_comment()
            .map(|comment| comment.id)
            && let Some(existing_index) = self
                .pending_review_comments
                .iter()
                .position(|comment| comment.id == existing_id)
        {
            self.pending_review_comments[existing_index].body = body;
            self.diff_selection_anchor = None;
            return Ok(());
        }

        let Some((line, start_line)) = self.selected_diff_comment_lines(side) else {
            return Err("selected range has no commentable lines");
        };
        let path = file.path.clone();

        if let Some(existing_index) = self.pending_review_comments.iter().position(|comment| {
            comment.path == path
                && comment.side == side
                && comment.line == line
                && comment.start_line == start_line
        }) {
            self.pending_review_comments[existing_index].body = body;
        } else if self.pending_range_overlaps_existing_comment(&path, side, line, start_line) {
            return Err("selection overlaps an existing pending comment");
        } else {
            self.pending_review_comments
                .push(PendingReviewCommentDraft {
                    id: self.next_pending_review_comment_id,
                    path,
                    side,
                    line,
                    start_line,
                    body,
                });
            self.next_pending_review_comment_id =
                self.next_pending_review_comment_id.saturating_add(1);
        }

        self.diff_selection_anchor = None;
        Ok(())
    }

    fn selected_diff_comment_side(&self) -> Option<PendingReviewCommentSide> {
        let file = self.selected_diff_file()?;
        let row = file.rows.get(self.selected_diff_line)?;
        if row.kind == crate::domain::PullRequestDiffRowKind::Context {
            return None;
        }

        if row.right_line_number.is_some() {
            Some(PendingReviewCommentSide::Right)
        } else if row.left_line_number.is_some() {
            Some(PendingReviewCommentSide::Left)
        } else {
            None
        }
    }

    fn selected_diff_comment_lines(
        &self,
        side: PendingReviewCommentSide,
    ) -> Option<(u64, Option<u64>)> {
        let file = self.selected_diff_file()?;
        if file.rows.is_empty() {
            return None;
        }

        let (start_row, end_row) = self
            .selected_diff_range()
            .unwrap_or((self.selected_diff_line, self.selected_diff_line));
        let start_row = start_row.min(file.rows.len().saturating_sub(1));
        let end_row = end_row.min(file.rows.len().saturating_sub(1));

        let mut lines = Vec::new();
        for row in file.rows.iter().take(end_row + 1).skip(start_row) {
            if let Some(line) = row_line_for_side(row, side) {
                lines.push(line as u64);
            }
        }
        if lines.is_empty() {
            return None;
        }

        lines.sort_unstable();
        let start = *lines.first()?;
        let end = *lines.last()?;
        let start_line = (start != end).then_some(start);
        Some((end, start_line))
    }

    fn row_overlaps_pending_comment(
        &self,
        row_index: usize,
        side: PendingReviewCommentSide,
    ) -> bool {
        let Some(file) = self.selected_diff_file() else {
            return false;
        };
        let Some(row) = file.rows.get(row_index) else {
            return false;
        };
        let Some(line) = row_line_for_side(row, side).map(|value| value as u64) else {
            return false;
        };

        self.pending_review_comments.iter().any(|comment| {
            if comment.path != file.path || comment.side != side {
                return false;
            }
            let (start, end) = pending_comment_bounds(comment.line, comment.start_line);
            line >= start && line <= end
        })
    }

    fn pending_range_overlaps_existing_comment(
        &self,
        path: &str,
        side: PendingReviewCommentSide,
        line: u64,
        start_line: Option<u64>,
    ) -> bool {
        let (selected_start, selected_end) = pending_comment_bounds(line, start_line);
        self.pending_review_comments.iter().any(|comment| {
            if comment.path != path || comment.side != side {
                return false;
            }
            let (existing_start, existing_end) =
                pending_comment_bounds(comment.line, comment.start_line);
            selected_start <= existing_end && existing_start <= selected_end
        })
    }

    fn move_selected_diff_line_down(&mut self) {
        let Some(file) = self.selected_diff_file() else {
            self.selected_diff_line = 0;
            self.diff_scroll = 0;
            return;
        };
        if file.rows.is_empty() {
            self.selected_diff_line = 0;
            self.diff_scroll = 0;
            return;
        }

        let max_index = file.rows.len().saturating_sub(1);
        let mut next = (self.selected_diff_line + 1).min(max_index);
        if self.diff_selection_anchor.is_some()
            && let Some(side) = self.selected_diff_comment_side()
        {
            let anchor = self
                .diff_selection_anchor
                .unwrap_or(self.selected_diff_line);
            let anchor_hunk = hunk_index_for_row(file, anchor);
            if let Some(row) =
                next_commentable_row(&file.rows, self.selected_diff_line, side, Direction::Down)
            {
                if hunk_index_for_row(file, row) == anchor_hunk {
                    if self.row_overlaps_pending_comment(row, side) {
                        next = self.selected_diff_line;
                    } else {
                        next = row;
                    }
                } else {
                    next = self.selected_diff_line;
                }
            } else {
                next = self.selected_diff_line;
            }
        }
        self.selected_diff_line = next;
        self.keep_selected_line_visible();
    }

    fn move_selected_diff_line_up(&mut self) {
        let mut next = self.selected_diff_line.saturating_sub(1);
        if self.diff_selection_anchor.is_some()
            && let Some(file) = self.selected_diff_file()
            && let Some(side) = self.selected_diff_comment_side()
        {
            let anchor = self
                .diff_selection_anchor
                .unwrap_or(self.selected_diff_line);
            let anchor_hunk = hunk_index_for_row(file, anchor);
            if let Some(row) =
                next_commentable_row(&file.rows, self.selected_diff_line, side, Direction::Up)
            {
                if hunk_index_for_row(file, row) == anchor_hunk {
                    if self.row_overlaps_pending_comment(row, side) {
                        next = self.selected_diff_line;
                    } else {
                        next = row;
                    }
                } else {
                    next = self.selected_diff_line;
                }
            } else {
                next = self.selected_diff_line;
            }
        }
        self.selected_diff_line = next;
        self.keep_selected_line_visible();
    }

    pub fn fast_scroll_down(&mut self) {
        if self.active_tab == ReviewTab::Threads {
            for _ in 0..10 {
                self.move_down();
            }
            return;
        }
        if self.active_tab != ReviewTab::Diff || self.diff_focus != DiffFocus::Content {
            return;
        }

        let steps = (usize::from(self.diff_viewport_height.max(2)) / 2).max(1);
        for _ in 0..steps {
            self.move_selected_diff_line_down();
        }
    }

    pub fn fast_scroll_up(&mut self) {
        if self.active_tab == ReviewTab::Threads {
            for _ in 0..10 {
                self.move_up();
            }
            return;
        }
        if self.active_tab != ReviewTab::Diff || self.diff_focus != DiffFocus::Content {
            return;
        }

        let steps = (usize::from(self.diff_viewport_height.max(2)) / 2).max(1);
        for _ in 0..steps {
            self.move_selected_diff_line_up();
        }
    }

    fn selection_is_commentable_for_side(&self, side: PendingReviewCommentSide) -> bool {
        let Some(file) = self.selected_diff_file() else {
            return false;
        };
        if file.rows.is_empty() {
            return false;
        }

        let (start_row, end_row) = self
            .selected_diff_range()
            .unwrap_or((self.selected_diff_line, self.selected_diff_line));
        let start_row = start_row.min(file.rows.len().saturating_sub(1));
        let end_row = end_row.min(file.rows.len().saturating_sub(1));

        file.rows
            .iter()
            .take(end_row + 1)
            .skip(start_row)
            .all(|row| is_commentable_diff_row(row, side))
    }

    fn keep_selected_line_visible(&mut self) {
        let viewport = usize::from(self.diff_viewport_height.max(1));
        let current_scroll = usize::from(self.diff_scroll);
        if self.selected_diff_line < current_scroll {
            self.diff_scroll = u16::try_from(self.selected_diff_line).unwrap_or(u16::MAX);
            return;
        }

        let lower_bound = current_scroll.saturating_add(viewport.saturating_sub(1));
        if self.selected_diff_line > lower_bound {
            let target_scroll = self
                .selected_diff_line
                .saturating_sub(viewport.saturating_sub(1));
            self.diff_scroll = u16::try_from(target_scroll).unwrap_or(u16::MAX);
        }
    }

    pub fn set_diff_viewport_height(&mut self, height: u16) {
        self.diff_viewport_height = height.max(1);
    }

    fn move_diff_tree_selection(&mut self, direction: NavDirection) {
        let Some((next_row, next_file)) = ({
            let rows = self.diff_tree_rows();
            if rows.is_empty() {
                None
            } else {
                let row = match direction {
                    NavDirection::Down => (self.selected_diff_row + 1).min(rows.len() - 1),
                    NavDirection::Up => self.selected_diff_row.saturating_sub(1),
                };
                Some((row, rows.get(row).and_then(|value| value.file_index)))
            }
        }) else {
            self.reset_diff_tree_selection();
            return;
        };

        self.selected_diff_row = next_row;
        if let Some(file_index) = next_file {
            self.set_selected_diff_file(file_index);
        }
    }

    fn reset_diff_tree_selection(&mut self) {
        self.selected_diff_row = 0;
        self.selected_diff_file = 0;
    }

    fn set_selected_diff_file(&mut self, file_index: usize) {
        if file_index == self.selected_diff_file {
            return;
        }

        self.selected_diff_file = file_index;
        self.diff_scroll = self.first_hunk_scroll(file_index);
        self.selected_hunk = 0;
        self.selected_diff_line = usize::from(self.diff_scroll);
        self.diff_selection_anchor = None;
    }

    fn first_hunk_scroll(&self, file_index: usize) -> u16 {
        self.diff
            .as_ref()
            .and_then(|diff| diff.files.get(file_index))
            .and_then(|file| file.hunk_starts.first().copied())
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(0)
    }

    fn realign_diff_selection_for_filter(&mut self) {
        let Some((row_index, file_index)) = ({
            let rows = self.diff_tree_rows();
            if rows.is_empty() {
                None
            } else {
                let current_file = rows
                    .get(self.selected_diff_row)
                    .and_then(|row| row.file_index)
                    .or_else(|| {
                        rows.iter()
                            .position(|row| row.file_index == Some(self.selected_diff_file))
                            .and_then(|index| rows.get(index).and_then(|row| row.file_index))
                    });

                if let Some(file_index) = current_file
                    && let Some(index) = rows
                        .iter()
                        .position(|row| row.file_index == Some(file_index))
                {
                    Some((index, Some(file_index)))
                } else {
                    rows.iter().enumerate().find_map(|(index, row)| {
                        row.file_index.map(|file_index| (index, Some(file_index)))
                    })
                }
            }
        }) else {
            self.clear_active_diff_file_selection();
            return;
        };

        self.selected_diff_row = row_index;
        if let Some(file_index) = file_index {
            self.set_selected_diff_file(file_index);
        } else {
            self.clear_active_diff_file_selection();
        }
    }

    fn clear_active_diff_file_selection(&mut self) {
        self.selected_diff_row = 0;
        self.selected_diff_file = 0;
        self.diff_scroll = 0;
        self.selected_hunk = 0;
        self.selected_diff_line = 0;
        self.diff_selection_anchor = None;
    }

    fn select_initial_diff_file_row(&mut self) {
        let Some((row_index, file_index)) = self
            .diff_tree_rows()
            .iter()
            .enumerate()
            .find_map(|(index, row)| row.file_index.map(|file_index| (index, file_index)))
        else {
            return;
        };

        self.selected_diff_row = row_index;
        self.selected_diff_file = file_index;
        self.selected_hunk = 0;
        self.diff_scroll = self.first_hunk_scroll(file_index);
        self.selected_diff_line = usize::from(self.diff_scroll);
        self.diff_selection_anchor = None;
    }

    fn set_active_hunk(&mut self, file_index: usize, hunk_start: usize, hunk_index: usize) {
        self.ensure_diff_file_expanded(file_index);
        self.selected_diff_file = file_index;
        self.selected_hunk = hunk_index;
        let viewport_height = usize::from(self.diff_viewport_height.max(1));
        let centered_start = hunk_start.saturating_sub(viewport_height / 2);
        let clamped_scroll = self
            .diff
            .as_ref()
            .and_then(|diff| diff.files.get(file_index))
            .map(|file| {
                if file.rows.is_empty() {
                    centered_start
                } else {
                    centered_start.min(file.rows.len().saturating_sub(viewport_height))
                }
            })
            .unwrap_or(centered_start);
        self.diff_scroll = u16::try_from(clamped_scroll).unwrap_or(u16::MAX);
        self.selected_diff_line = hunk_start;
        self.diff_selection_anchor = None;

        if let Some(row_index) = self
            .diff_tree_rows()
            .iter()
            .position(|row| row.file_index == Some(file_index))
        {
            self.selected_diff_row = row_index;
        }
    }

    fn pending_comment_locations(&self) -> Vec<PendingCommentLocation> {
        let Some(diff) = self.diff.as_ref() else {
            return Vec::new();
        };

        let mut locations = self
            .pending_review_comments
            .iter()
            .filter_map(|comment| {
                let file_index = diff
                    .files
                    .iter()
                    .position(|file| file.path == comment.path)?;
                let file = diff.files.get(file_index)?;
                let row_index = pending_comment_row_index(file, comment)?;
                Some(PendingCommentLocation {
                    comment_id: comment.id,
                    file_index,
                    row_index,
                })
            })
            .collect::<Vec<_>>();

        locations.sort_unstable_by_key(|location| {
            (location.file_index, location.row_index, location.comment_id)
        });
        locations.dedup_by_key(|location| (location.file_index, location.row_index));
        locations
    }

    fn jump_to_pending_comment_location(&mut self, location: PendingCommentLocation) {
        self.ensure_diff_file_expanded(location.file_index);
        self.set_selected_diff_file(location.file_index);
        self.selected_diff_line = location.row_index;
        self.diff_selection_anchor = None;

        if let Some(file) = self.selected_diff_file() {
            self.selected_hunk = hunk_index_for_row(file, location.row_index).unwrap_or(0);
        }
        self.keep_selected_line_visible();

        if let Some(row_index) = self
            .diff_tree_rows()
            .iter()
            .position(|row| row.file_index == Some(location.file_index))
        {
            self.selected_diff_row = row_index;
        }
    }

    fn ensure_diff_file_expanded(&mut self, file_index: usize) {
        if !self.diff_search.query().trim().is_empty() {
            return;
        }

        let Some(path) = self
            .diff
            .as_ref()
            .and_then(|diff| diff.files.get(file_index))
            .map(|file| file.path.clone())
        else {
            return;
        };

        let mut key = String::new();
        let parts = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();

        let mut changed = false;
        for segment in parts.iter().take(parts.len().saturating_sub(1)) {
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(segment);
            changed |= self.diff_collapsed_dirs.remove(&key);
        }

        if changed {
            self.recompute_diff_tree_rows_cache();
        }
    }

    fn navigable_diff_files(&self) -> Vec<usize> {
        let rows = self.diff_tree_rows();
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        for row in rows {
            let Some(file_index) = row.file_index else {
                continue;
            };
            if seen.insert(file_index) {
                files.push(file_index);
            }
        }

        if !files.is_empty() {
            return files;
        }

        self.diff
            .as_ref()
            .map(|diff| {
                diff.files
                    .iter()
                    .enumerate()
                    .filter_map(|(index, file)| (!file.hunk_starts.is_empty()).then_some(index))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn recompute_diff_tree_rows_cache(&mut self) {
        let Some(diff) = self.diff.as_ref() else {
            self.diff_tree_rows_cache.clear();
            return;
        };

        self.diff_tree_rows_cache = if self.diff_search.is_empty() {
            build_diff_tree_rows(diff, &self.diff_collapsed_dirs)
        } else {
            let expanded_rows = build_diff_tree_rows(diff, &HashSet::new());
            filter_diff_tree_rows(&expanded_rows, diff, self.diff_search.query())
        };
    }
}

/// Active sub-view in the review screen.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ReviewTab {
    Threads,
    Diff,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DiffFocus {
    Files,
    Content,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PendingReviewCommentSide {
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub struct PendingReviewCommentDraft {
    pub id: u64,
    pub path: String,
    pub side: PendingReviewCommentSide,
    pub line: u64,
    pub start_line: Option<u64>,
    pub body: String,
}

#[derive(Debug, Clone, Copy)]
struct PendingCommentLocation {
    comment_id: u64,
    file_index: usize,
    row_index: usize,
}

#[derive(Debug, Clone)]
pub struct DiffTreeRow {
    pub key: String,
    pub label: String,
    pub depth: usize,
    pub is_directory: bool,
    pub is_collapsed: bool,
    pub file_index: Option<usize>,
}

fn row_line_for_side(
    row: &crate::domain::PullRequestDiffRow,
    side: PendingReviewCommentSide,
) -> Option<usize> {
    match side {
        PendingReviewCommentSide::Left => row.left_line_number,
        PendingReviewCommentSide::Right => row.right_line_number,
    }
}

fn pending_comment_bounds(line: u64, start_line: Option<u64>) -> (u64, u64) {
    let start = start_line.unwrap_or(line).min(line);
    let end = start_line.unwrap_or(line).max(line);
    (start, end)
}

fn pending_comment_contains_line(comment: &PendingReviewCommentDraft, line: u64) -> bool {
    let (start, end) = pending_comment_bounds(comment.line, comment.start_line);
    line >= start && line <= end
}

fn pending_comment_matches_current_diff(
    diff: &crate::domain::PullRequestDiffData,
    comment: &PendingReviewCommentDraft,
) -> bool {
    let Some(file) = diff.files.iter().find(|file| file.path == comment.path) else {
        return false;
    };

    let start = comment.start_line.unwrap_or(comment.line).min(comment.line);
    let end = comment.start_line.unwrap_or(comment.line).max(comment.line);
    let required = end.saturating_sub(start).saturating_add(1) as usize;
    let mut matched = 0usize;

    for row in &file.rows {
        if !is_commentable_diff_row(row, comment.side) {
            continue;
        }
        let Some(line) = row_line_for_side(row, comment.side).map(|line| line as u64) else {
            continue;
        };
        if line >= start && line <= end {
            matched = matched.saturating_add(1);
            if matched >= required {
                return true;
            }
        }
    }

    false
}

fn pending_comment_row_index(
    file: &PullRequestDiffFile,
    comment: &PendingReviewCommentDraft,
) -> Option<usize> {
    let start = comment.start_line.unwrap_or(comment.line).min(comment.line);
    let end = comment.start_line.unwrap_or(comment.line).max(comment.line);
    file.rows.iter().position(|row| {
        let Some(line) = row_line_for_side(row, comment.side).map(|line| line as u64) else {
            return false;
        };
        line >= start && line <= end
    })
}

fn hunk_index_for_row(file: &PullRequestDiffFile, row_index: usize) -> Option<usize> {
    if file.hunk_starts.is_empty() {
        return None;
    }
    if row_index < file.hunk_starts[0] {
        return None;
    }

    let position = file
        .hunk_starts
        .partition_point(|start| *start <= row_index);
    position.checked_sub(1)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Direction {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum NavDirection {
    Up,
    Down,
}

fn next_commentable_row(
    rows: &[crate::domain::PullRequestDiffRow],
    from: usize,
    side: PendingReviewCommentSide,
    direction: Direction,
) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }

    let next_index = match direction {
        Direction::Down => from.saturating_add(1),
        Direction::Up => from.saturating_sub(1),
    };
    if next_index >= rows.len() || (direction == Direction::Up && from == 0) {
        return None;
    }

    let row = rows.get(next_index)?;
    is_commentable_diff_row(row, side).then_some(next_index)
}

fn is_commentable_diff_row(
    row: &crate::domain::PullRequestDiffRow,
    side: PendingReviewCommentSide,
) -> bool {
    if row.kind == crate::domain::PullRequestDiffRowKind::Context {
        return false;
    }
    row_line_for_side(row, side).is_some()
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, DiffFocus, PendingReviewCommentDraft, ReviewScreenState, ReviewTab,
        build_diff_tree_rows,
    };
    use crate::domain::{
        PullRequestComment, PullRequestData, PullRequestDiffData, PullRequestDiffFile,
        PullRequestDiffFileStatus, PullRequestDiffRow, PullRequestDiffRowKind, PullRequestSummary,
        ReviewComment, ReviewThread,
    };
    use serde_json::json;
    use std::collections::HashSet;

    fn diff_file(path: &str) -> PullRequestDiffFile {
        PullRequestDiffFile {
            path: path.to_owned(),
            status: PullRequestDiffFileStatus::Modified,
            rows: Vec::new(),
            hunk_starts: Vec::new(),
        }
    }

    fn context_row() -> PullRequestDiffRow {
        PullRequestDiffRow {
            left_line_number: None,
            right_line_number: None,
            left_text: String::new(),
            right_text: String::new(),
            left_highlights: Vec::new(),
            right_highlights: Vec::new(),
            kind: PullRequestDiffRowKind::Context,
        }
    }

    fn paired_row(left: usize, right: usize) -> PullRequestDiffRow {
        PullRequestDiffRow {
            left_line_number: Some(left),
            right_line_number: Some(right),
            left_text: format!("left-{left}"),
            right_text: format!("right-{right}"),
            left_highlights: Vec::new(),
            right_highlights: Vec::new(),
            kind: PullRequestDiffRowKind::Modified,
        }
    }

    fn numbered_context_row(left: usize, right: usize) -> PullRequestDiffRow {
        PullRequestDiffRow {
            left_line_number: Some(left),
            right_line_number: Some(right),
            left_text: format!("left-{left}"),
            right_text: format!("right-{right}"),
            left_highlights: Vec::new(),
            right_highlights: Vec::new(),
            kind: PullRequestDiffRowKind::Context,
        }
    }

    fn build_review_state() -> ReviewScreenState {
        build_review_state_with_comments(Vec::new())
    }

    fn build_review_state_with_comments(comments: Vec<PullRequestComment>) -> ReviewScreenState {
        let pull = PullRequestSummary {
            owner: "owner".to_owned(),
            repo: "repo".to_owned(),
            number: 42,
            title: "Example".to_owned(),
            author: "dev".to_owned(),
            head_ref: "feature".to_owned(),
            base_ref: "main".to_owned(),
            head_sha: "headsha".to_owned(),
            base_sha: "basesha".to_owned(),
            html_url: Some("https://example.com".to_owned()),
            updated_at_unix_ms: 0,
            created_at_unix_ms: 0,
            is_draft: false,
            reviewer_logins: Vec::new(),
            review_status: None,
        };

        let data = PullRequestData {
            head_ref: "feature".to_owned(),
            base_ref: "main".to_owned(),
            head_sha: "headsha".to_owned(),
            base_sha: "basesha".to_owned(),
            changed_files: Vec::new(),
            comments,
            load_warning: None,
        };

        ReviewScreenState::new(pull, data)
    }

    fn review_comment(id: u64, body: &str, in_reply_to_id: Option<u64>) -> ReviewComment {
        let mut payload = json!({
            "url": format!("https://example.invalid/comments/{id}"),
            "id": id,
            "node_id": format!("PRRC_{id}"),
            "diff_hunk": "@@ -1,1 +1,1 @@",
            "path": "src/lib.rs",
            "commit_id": "deadbeef",
            "original_commit_id": "deadbeef",
            "body": body,
            "created_at": "2026-02-01T00:00:00Z",
            "updated_at": "2026-02-01T00:00:00Z",
            "html_url": format!("https://example.invalid/comments/{id}"),
            "_links": {},
            "line": 1
        });

        if let Some(reply_to) = in_reply_to_id {
            payload["in_reply_to_id"] = json!(reply_to);
        }

        serde_json::from_value(payload).expect("valid pull review comment fixture")
    }

    fn review_thread_with_reply(
        root_id: u64,
        root_body: &str,
        reply_id: u64,
        reply_body: &str,
    ) -> ReviewThread {
        ReviewThread {
            thread_id: Some(format!("THREAD_{root_id}")),
            is_resolved: false,
            comment: review_comment(root_id, root_body, None),
            replies: vec![ReviewThread {
                thread_id: None,
                is_resolved: false,
                comment: review_comment(reply_id, reply_body, Some(root_id)),
                replies: Vec::new(),
            }],
        }
    }

    #[test]
    fn compresses_single_directory_chain() {
        let diff = PullRequestDiffData {
            files: vec![
                diff_file("one/two/three/a.rs"),
                diff_file("one/two/three/b.rs"),
                diff_file("one/two/three/c.rs"),
            ],
        };

        let rows = build_diff_tree_rows(&diff, &HashSet::new());
        assert!(!rows.is_empty());
        assert!(rows[0].is_directory);
        assert_eq!(rows[0].label, "one/two/three");
        assert_eq!(rows[0].depth, 0);
    }

    #[test]
    fn does_not_compress_across_branching_directory() {
        let diff = PullRequestDiffData {
            files: vec![diff_file("one/two/a.rs"), diff_file("one/three/b.rs")],
        };

        let rows = build_diff_tree_rows(&diff, &HashSet::new());
        assert!(!rows.is_empty());
        assert!(rows[0].is_directory);
        assert_eq!(rows[0].label, "one");
    }

    #[test]
    fn set_diff_selects_first_tree_file() {
        let mut review = build_review_state();
        let mut first = diff_file("alpha.rs");
        let mut second = diff_file("beta.rs");
        first.hunk_starts = vec![];
        second.hunk_starts = vec![12, 40];

        review.set_diff(PullRequestDiffData {
            files: vec![first, second],
        });

        assert_eq!(review.selected_diff_file, 0);
        assert_eq!(review.diff_scroll, 0);
        assert_eq!(review.selected_hunk, 0);
    }

    #[test]
    fn set_diff_scrolls_to_first_hunk_in_first_tree_file() {
        let mut review = build_review_state();
        let mut first = diff_file("alpha.rs");
        let mut second = diff_file("beta.rs");
        first.hunk_starts = vec![8, 30];
        second.hunk_starts = vec![3];

        review.set_diff(PullRequestDiffData {
            files: vec![first, second],
        });

        assert_eq!(review.selected_diff_file, 0);
        assert_eq!(review.diff_scroll, 8);
        assert_eq!(review.selected_hunk, 0);
    }

    #[test]
    fn jump_next_hunk_wraps_to_next_file() {
        let mut review = build_review_state();
        let mut first = diff_file("alpha.rs");
        let mut second = diff_file("beta.rs");
        first.hunk_starts = vec![4, 9];
        second.hunk_starts = vec![3];

        review.set_diff(PullRequestDiffData {
            files: vec![first, diff_file("no_hunks.rs"), second],
        });
        review.selected_diff_file = 0;
        review.diff_scroll = 9;
        review.selected_diff_line = 9;

        review.jump_next_hunk();

        assert_eq!(review.selected_diff_file, 2);
        assert_eq!(review.diff_scroll, 3);
        assert_eq!(review.selected_hunk, 0);
    }

    #[test]
    fn jump_prev_hunk_wraps_to_previous_file() {
        let mut review = build_review_state();
        let mut first = diff_file("alpha.rs");
        let mut second = diff_file("beta.rs");
        first.hunk_starts = vec![4, 9];
        second.hunk_starts = vec![3, 8];

        review.set_diff(PullRequestDiffData {
            files: vec![first, diff_file("no_hunks.rs"), second],
        });
        review.selected_diff_file = 0;
        review.diff_scroll = 4;
        review.selected_diff_line = 4;

        review.jump_prev_hunk();

        assert_eq!(review.selected_diff_file, 2);
        assert_eq!(review.diff_scroll, 8);
        assert_eq!(review.selected_hunk, 1);
    }

    #[test]
    fn jump_next_hunk_centers_target_in_viewport() {
        let mut review = build_review_state();
        let mut first = diff_file("alpha.rs");
        first.hunk_starts = vec![60];
        first.rows = vec![context_row(); 200];

        review.set_diff(PullRequestDiffData { files: vec![first] });
        review.set_diff_viewport_height(20);
        review.diff_scroll = 0;

        review.jump_next_hunk();

        assert_eq!(review.selected_diff_file, 0);
        assert_eq!(review.selected_hunk, 0);
        assert_eq!(review.diff_scroll, 50);
    }

    #[test]
    fn move_file_selection_jumps_to_first_hunk() {
        let mut review = build_review_state();
        let mut first = diff_file("alpha.rs");
        let mut second = diff_file("beta.rs");
        first.hunk_starts = vec![4, 20];
        second.hunk_starts = vec![13, 44];

        review.set_diff(PullRequestDiffData {
            files: vec![first, second],
        });
        review.active_tab = ReviewTab::Diff;

        review.move_down();
        assert_eq!(review.selected_diff_file, 1);
        assert_eq!(review.selected_hunk, 0);
        assert_eq!(review.diff_scroll, 13);

        review.move_up();
        assert_eq!(review.selected_diff_file, 0);
        assert_eq!(review.selected_hunk, 0);
        assert_eq!(review.diff_scroll, 4);
    }

    #[test]
    fn diff_search_filters_rows_and_updates_selection() {
        let mut review = build_review_state();
        review.set_diff(PullRequestDiffData {
            files: vec![diff_file("src/lib.rs"), diff_file("docs/readme.md")],
        });

        review.diff_search_input_mut().push_char('d');
        review.refresh_diff_search_results();
        review.diff_search_input_mut().push_char('o');
        review.refresh_diff_search_results();

        let rows = review.diff_tree_rows();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| {
            row.is_directory || row.key.contains("docs/readme.md") || row.key.contains("docs")
        }));
        assert_eq!(review.selected_diff_file, 1);
    }

    #[test]
    fn thread_search_filters_rows_and_keeps_parent_context() {
        let review_thread = review_thread_with_reply(1, "root message", 2, "needle reply text");
        let mut review = build_review_state_with_comments(vec![PullRequestComment::ReviewThread(
            Box::new(review_thread),
        )]);

        for ch in "needle".chars() {
            review.thread_search_input_mut().push_char(ch);
            review.refresh_thread_search_results();
        }

        assert_eq!(review.nodes.len(), 2);
        assert_eq!(review.nodes[0].depth, 0);
        assert_eq!(review.nodes[1].depth, 1);
        assert!(review.nodes[1].comment.body().contains("needle"));
    }

    #[test]
    fn thread_search_realigns_selection_when_current_row_is_filtered_out() {
        let first = ReviewThread {
            thread_id: Some("THREAD_1".to_owned()),
            is_resolved: false,
            comment: review_comment(1, "alpha root", None),
            replies: Vec::new(),
        };
        let second = ReviewThread {
            thread_id: Some("THREAD_2".to_owned()),
            is_resolved: false,
            comment: review_comment(2, "beta root", None),
            replies: Vec::new(),
        };
        let mut review = build_review_state_with_comments(vec![
            PullRequestComment::ReviewThread(Box::new(first)),
            PullRequestComment::ReviewThread(Box::new(second)),
        ]);

        review.move_down();
        assert_eq!(review.selected_row, 1);

        for ch in "alpha".chars() {
            review.thread_search_input_mut().push_char(ch);
            review.refresh_thread_search_results();
        }

        assert_eq!(review.nodes.len(), 1);
        assert_eq!(review.selected_row, 0);
        assert!(
            review
                .selected_node()
                .is_some_and(|node| node.key.contains("THREAD_1"))
        );
    }

    #[test]
    fn pending_comment_from_selected_line_prefers_right_side() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(5, 8)];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });

        review
            .upsert_pending_review_comment_from_selection("review me".to_owned())
            .expect("pending comment should be staged");

        let pending = review.pending_review_comments();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].path, "alpha.rs");
        assert_eq!(pending[0].side, super::PendingReviewCommentSide::Right);
        assert_eq!(pending[0].line, 8);
        assert_eq!(pending[0].start_line, None);
    }

    #[test]
    fn pending_comment_from_range_sets_start_line() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(10, 20), paired_row(11, 21), paired_row(12, 22)];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });

        review.diff_selection_anchor = Some(0);
        review.selected_diff_line = 2;

        review
            .upsert_pending_review_comment_from_selection("range".to_owned())
            .expect("pending range comment should be staged");

        let pending = review.pending_review_comments();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].line, 22);
        assert_eq!(pending[0].start_line, Some(20));
    }

    #[test]
    fn editing_inside_pending_range_updates_existing_comment() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(10, 20), paired_row(11, 21), paired_row(12, 22)];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });

        review.diff_selection_anchor = Some(0);
        review.selected_diff_line = 2;
        review
            .upsert_pending_review_comment_from_selection("original".to_owned())
            .expect("pending range comment should be staged");
        assert_eq!(review.pending_review_comment_count(), 1);

        // Cursor inside the existing range, but without selecting the full range.
        review.diff_selection_anchor = None;
        review.selected_diff_line = 1;
        review
            .upsert_pending_review_comment_from_selection("edited".to_owned())
            .expect("editing inside range should update existing comment");

        let pending = review.pending_review_comments();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "edited");
        assert_eq!(pending[0].line, 22);
        assert_eq!(pending[0].start_line, Some(20));
    }

    #[test]
    fn deleting_inside_pending_range_removes_existing_comment() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(10, 20), paired_row(11, 21), paired_row(12, 22)];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });

        review.diff_selection_anchor = Some(0);
        review.selected_diff_line = 2;
        review
            .upsert_pending_review_comment_from_selection("original".to_owned())
            .expect("pending range comment should be staged");
        assert_eq!(review.pending_review_comment_count(), 1);

        // Cursor inside the existing range, but without selecting the full range.
        review.diff_selection_anchor = None;
        review.selected_diff_line = 1;
        assert!(review.remove_selected_pending_review_comment());
        assert_eq!(review.pending_review_comment_count(), 0);
    }

    #[test]
    fn set_data_keeps_pending_inline_comments_when_head_changes() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(10, 20)];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });

        review
            .upsert_pending_review_comment_from_selection("staged".to_owned())
            .expect("pending comment should be staged");
        assert_eq!(review.pending_review_comment_count(), 1);

        let mut refreshed = review.data.clone();
        refreshed.head_sha = "new-head-sha".to_owned();

        let changed = review.set_data(refreshed);

        assert!(changed);
        assert_eq!(review.pending_review_comment_count(), 1);
        assert_eq!(review.pull.head_sha, "new-head-sha");
    }

    #[test]
    fn pending_comments_are_marked_outdated_when_diff_no_longer_matches() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(10, 20)];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });

        review
            .upsert_pending_review_comment_from_selection("staged".to_owned())
            .expect("pending comment should be staged");
        assert_eq!(review.pending_review_comment_count(), 1);

        let mut refreshed = diff_file("alpha.rs");
        refreshed.rows = vec![paired_row(30, 40)];
        refreshed.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData {
            files: vec![refreshed],
        });

        let pending = review.pending_review_comments();
        assert_eq!(pending.len(), 1);
        assert!(review.pending_review_comment_is_outdated(&pending[0]));
    }

    #[test]
    fn jump_next_pending_comment_wraps_across_files() {
        let mut review = build_review_state();
        let mut first = diff_file("alpha.rs");
        first.rows = vec![paired_row(10, 20)];
        first.hunk_starts = vec![0];
        let mut second = diff_file("beta.rs");
        second.rows = vec![paired_row(30, 40)];
        second.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData {
            files: vec![first, second],
        });

        review.pending_review_comments = vec![
            PendingReviewCommentDraft {
                id: 1,
                path: "alpha.rs".to_owned(),
                side: super::PendingReviewCommentSide::Right,
                line: 20,
                start_line: None,
                body: "first".to_owned(),
            },
            PendingReviewCommentDraft {
                id: 2,
                path: "beta.rs".to_owned(),
                side: super::PendingReviewCommentSide::Right,
                line: 40,
                start_line: None,
                body: "second".to_owned(),
            },
        ];

        review.selected_diff_file = 0;
        review.selected_diff_line = 0;
        assert!(review.jump_next_pending_review_comment());
        assert_eq!(review.selected_diff_file, 1);
        assert_eq!(review.selected_diff_line, 0);

        assert!(review.jump_next_pending_review_comment());
        assert_eq!(review.selected_diff_file, 0);
        assert_eq!(review.selected_diff_line, 0);
    }

    #[test]
    fn jump_prev_pending_comment_wraps_across_files() {
        let mut review = build_review_state();
        let mut first = diff_file("alpha.rs");
        first.rows = vec![paired_row(10, 20)];
        first.hunk_starts = vec![0];
        let mut second = diff_file("beta.rs");
        second.rows = vec![paired_row(30, 40)];
        second.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData {
            files: vec![first, second],
        });

        review.pending_review_comments = vec![
            PendingReviewCommentDraft {
                id: 1,
                path: "alpha.rs".to_owned(),
                side: super::PendingReviewCommentSide::Right,
                line: 20,
                start_line: None,
                body: "first".to_owned(),
            },
            PendingReviewCommentDraft {
                id: 2,
                path: "beta.rs".to_owned(),
                side: super::PendingReviewCommentSide::Right,
                line: 40,
                start_line: None,
                body: "second".to_owned(),
            },
        ];

        review.selected_diff_file = 0;
        review.selected_diff_line = 0;
        assert!(review.jump_prev_pending_review_comment());
        assert_eq!(review.selected_diff_file, 1);
        assert_eq!(review.selected_diff_line, 0);
    }

    #[test]
    fn visual_selection_does_not_start_on_context_row() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![numbered_context_row(1, 1), paired_row(2, 2)];
        file.hunk_starts = vec![1];
        review.set_diff(PullRequestDiffData { files: vec![file] });

        review.selected_diff_line = 0;
        review.toggle_diff_selection_anchor();

        assert!(!review.has_diff_selection_anchor());
    }

    #[test]
    fn visual_selection_does_not_start_on_existing_pending_comment_range() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(10, 20), paired_row(11, 21)];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });
        review.active_tab = ReviewTab::Diff;
        review.diff_focus = DiffFocus::Content;
        review.selected_diff_line = 0;

        review
            .upsert_pending_review_comment_from_selection("existing".to_owned())
            .expect("pending comment should be staged");

        review.selected_diff_line = 0;
        review.toggle_diff_selection_anchor();

        assert!(!review.has_diff_selection_anchor());
    }

    #[test]
    fn visual_selection_does_not_expand_into_non_commentable_rows() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![
            paired_row(10, 20),
            numbered_context_row(11, 21),
            paired_row(12, 22),
        ];
        file.hunk_starts = vec![0, 2];
        review.set_diff(PullRequestDiffData { files: vec![file] });
        review.active_tab = ReviewTab::Diff;
        review.diff_focus = DiffFocus::Content;
        review.selected_diff_line = 0;

        review.toggle_diff_selection_anchor();
        review.move_down();

        assert_eq!(review.selected_diff_line, 0);
        assert_eq!(review.selected_diff_range(), Some((0, 0)));
    }

    #[test]
    fn visual_selection_does_not_expand_into_existing_pending_comment_range() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(10, 20), paired_row(11, 21), paired_row(12, 22)];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });
        review.active_tab = ReviewTab::Diff;
        review.diff_focus = DiffFocus::Content;

        review.selected_diff_line = 1;
        review
            .upsert_pending_review_comment_from_selection("existing".to_owned())
            .expect("pending comment should be staged");

        review.selected_diff_line = 0;
        review.toggle_diff_selection_anchor();
        review.move_down();

        assert_eq!(review.selected_diff_line, 0);
        assert_eq!(review.selected_diff_range(), Some((0, 0)));
    }

    #[test]
    fn pending_comment_selection_cannot_overlap_existing_pending_comment() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![
            paired_row(10, 20),
            paired_row(11, 21),
            paired_row(12, 22),
            paired_row(13, 23),
        ];
        file.hunk_starts = vec![0];
        review.set_diff(PullRequestDiffData { files: vec![file] });
        review.active_tab = ReviewTab::Diff;
        review.diff_focus = DiffFocus::Content;

        review.diff_selection_anchor = Some(1);
        review.selected_diff_line = 2;
        review
            .upsert_pending_review_comment_from_selection("existing".to_owned())
            .expect("pending comment should be staged");
        assert_eq!(review.pending_review_comment_count(), 1);

        review.diff_selection_anchor = Some(0);
        review.selected_diff_line = 3;
        let result = review.upsert_pending_review_comment_from_selection("overlap".to_owned());

        assert_eq!(
            result,
            Err("selection overlaps an existing pending comment")
        );
        assert_eq!(review.pending_review_comment_count(), 1);
    }

    #[test]
    fn visual_selection_does_not_cross_hunk_boundary() {
        let mut review = build_review_state();
        let mut file = diff_file("alpha.rs");
        file.rows = vec![paired_row(10, 20), paired_row(11, 21)];
        file.hunk_starts = vec![0, 1];
        review.set_diff(PullRequestDiffData { files: vec![file] });
        review.active_tab = ReviewTab::Diff;
        review.diff_focus = DiffFocus::Content;
        review.selected_diff_line = 0;

        review.toggle_diff_selection_anchor();
        review.move_down();

        assert_eq!(review.selected_diff_line, 0);
        assert_eq!(review.selected_diff_range(), Some((0, 0)));
    }

    #[test]
    fn search_scope_author_limits_results_to_viewer_login() {
        let mut state = AppState::default();
        state.set_viewer_login(Some("alice".to_owned()));
        state.set_pull_requests(vec![
            search_pull(1, "alice", 10, 1),
            search_pull(2, "bob", 20, 2),
            search_pull(3, "ALICE", 30, 3),
        ]);

        state.toggle_search_scope();

        let numbers: Vec<u64> = state
            .search_results
            .iter()
            .filter_map(|index| state.pull_requests.get(*index))
            .map(|pull| pull.number)
            .collect();
        assert_eq!(numbers, vec![3, 1]);
    }

    #[test]
    fn search_sort_created_orders_by_creation_timestamp_desc() {
        let mut state = AppState::default();
        state.set_pull_requests(vec![
            search_pull(1, "alice", 10, 300),
            search_pull(2, "alice", 30, 100),
            search_pull(3, "alice", 20, 200),
        ]);

        state.toggle_search_sort();

        let numbers: Vec<u64> = state
            .search_results
            .iter()
            .filter_map(|index| state.pull_requests.get(*index))
            .map(|pull| pull.number)
            .collect();
        assert_eq!(numbers, vec![1, 3, 2]);
    }

    #[test]
    fn search_status_filter_approved_only_keeps_approved_pulls() {
        let mut state = AppState::default();
        state.set_pull_requests(vec![
            search_pull_with_status(1, false, None),
            search_pull_with_status(
                2,
                false,
                Some(crate::domain::PullRequestReviewStatus::Approved),
            ),
            search_pull_with_status(
                3,
                false,
                Some(crate::domain::PullRequestReviewStatus::ChangesRequested),
            ),
        ]);

        state.toggle_search_status_filter(); // draft
        state.toggle_search_status_filter(); // ready
        state.toggle_search_status_filter(); // approved

        let numbers: Vec<u64> = state
            .search_results
            .iter()
            .filter_map(|index| state.pull_requests.get(*index))
            .map(|pull| pull.number)
            .collect();
        assert_eq!(numbers, vec![2]);
    }

    fn search_pull(
        number: u64,
        author: &str,
        updated_at_unix_ms: i64,
        created_at_unix_ms: i64,
    ) -> PullRequestSummary {
        PullRequestSummary {
            owner: "owner".to_owned(),
            repo: "repo".to_owned(),
            number,
            title: format!("PR {number}"),
            author: author.to_owned(),
            head_ref: format!("feature/{number}"),
            base_ref: "main".to_owned(),
            head_sha: format!("{number:040x}"),
            base_sha: format!("{:040x}", number + 1),
            html_url: None,
            updated_at_unix_ms,
            created_at_unix_ms,
            is_draft: false,
            reviewer_logins: Vec::new(),
            review_status: None,
        }
    }

    fn search_pull_with_status(
        number: u64,
        is_draft: bool,
        review_status: Option<crate::domain::PullRequestReviewStatus>,
    ) -> PullRequestSummary {
        PullRequestSummary {
            owner: "owner".to_owned(),
            repo: "repo".to_owned(),
            number,
            title: format!("PR {number}"),
            author: "alice".to_owned(),
            head_ref: format!("feature/{number}"),
            base_ref: "main".to_owned(),
            head_sha: format!("{number:040x}"),
            base_sha: format!("{:040x}", number + 1),
            html_url: None,
            updated_at_unix_ms: number as i64,
            created_at_unix_ms: number as i64,
            is_draft,
            reviewer_logins: Vec::new(),
            review_status,
        }
    }
}
