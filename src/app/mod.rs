//! Application runtime, event loop, and keyboard handling.

pub mod browser;
pub mod drafts;
pub mod editor;
pub mod events;
pub mod state;

use crate::{
    app::{
        drafts::{DraftStore, LoadOutcome},
        events::{
            MutationRequest, WorkerMessage, spawn_apply_mutation, spawn_load_pull_request_data,
            spawn_load_pull_request_diff, spawn_load_pull_requests,
            spawn_load_specific_pull_request, spawn_load_viewer_login,
        },
        state::{
            AppState, PendingReviewCommentSide, ReviewSubmissionEvent, ReviewTab, SearchInputState,
        },
    },
    config,
    domain::{CommentRef, PullRequestSummary, Route},
    github::{client::create_client, comments::SubmitReviewComment},
    render::markdown::MarkdownRenderer,
    ui,
    ui::theme::{self, ThemeMode},
};
use anyhow::Context;
use browser::open_in_browser;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use octocrab::models::pulls;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{Stdout, stdout},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

type WorkerTx = UnboundedSender<WorkerMessage>;

/// Runtime configuration provided by CLI flags.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub pull: Option<u64>,
    pub initial_theme_mode: ThemeMode,
    pub initial_terminal_background: Option<(u8, u8, u8)>,
    pub theme_config: config::AppConfig,
}

struct DataContext {
    client: octocrab::Octocrab,
    owner: Option<String>,
    repo: Option<String>,
}

struct EventLoopDependencies<'a> {
    context: &'a DataContext,
    config: &'a AppConfig,
    tx: &'a WorkerTx,
    rx: &'a mut UnboundedReceiver<WorkerMessage>,
    markdown: &'a mut MarkdownRenderer,
    draft_store: Option<&'a DraftStore>,
}

/// Runs the interactive TUI application.
pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMessage>();

    let mut state = AppState::default();
    let draft_store = match DraftStore::new().await {
        Ok(store) => Some(store),
        Err(err) => {
            state.error_message = Some(format!("draft persistence unavailable: {err}"));
            None
        }
    };

    let client = create_client()
        .await
        .context("failed to create authenticated GitHub client")?;

    spawn_load_viewer_login(tx.clone(), client.clone());

    if let Some(pull_number) = config.pull {
        state.begin_operation(format!("Loading pull request #{pull_number}"));
        spawn_load_specific_pull_request(
            tx.clone(),
            client.clone(),
            config.owner.clone(),
            config.repo.clone(),
            pull_number,
        );
    } else {
        state.begin_operation("Loading open pull requests");
        spawn_load_pull_requests(
            tx.clone(),
            client.clone(),
            config.owner.clone(),
            config.repo.clone(),
        );
    }

    let context = DataContext {
        client,
        owner: config.owner.clone(),
        repo: config.repo.clone(),
    };

    let mut terminal = setup_terminal()?;
    let mut markdown = MarkdownRenderer::new();
    let _ = markdown.set_ocean_theme(config.initial_theme_mode);

    let mut deps = EventLoopDependencies {
        context: &context,
        config: &config,
        tx: &tx,
        rx: &mut rx,
        markdown: &mut markdown,
        draft_store: draft_store.as_ref(),
    };
    let result = run_event_loop(&mut terminal, &mut state, &mut deps).await;

    restore_terminal(&mut terminal)?;
    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    deps: &mut EventLoopDependencies<'_>,
) -> anyhow::Result<()> {
    let mut active_theme_mode = deps.config.initial_theme_mode;
    let mut active_terminal_background = deps.config.initial_terminal_background;
    let mut last_theme_poll = Instant::now();
    let mut last_user_input = Instant::now();
    let mut last_error_snapshot: Option<String> = None;
    let mut last_error_at: Option<Instant> = None;
    let mut last_persisted_draft_signature: Option<String> = None;

    loop {
        state.advance_spinner();

        while let Ok(message) = deps.rx.try_recv() {
            process_worker_message(
                state,
                message,
                deps.markdown,
                deps.context,
                deps.tx,
                deps.draft_store,
                &mut last_persisted_draft_signature,
            )
            .await;
            maybe_spawn_search_load(state, deps.context, deps.tx);
            load_active_diff_if_needed(state, deps.tx);
            // Persist immediately after worker-driven mutations (for example submit review).
            persist_drafts_if_enabled(state, deps.draft_store, &mut last_persisted_draft_signature)
                .await;
        }

        if state.error_message != last_error_snapshot {
            last_error_snapshot = state.error_message.clone();
            last_error_at = state.error_message.as_ref().map(|_| Instant::now());
        } else if state.error_message.is_some()
            && last_error_at.is_some_and(|seen_at| seen_at.elapsed() >= Duration::from_secs(10))
        {
            state.error_message = None;
            last_error_snapshot = None;
            last_error_at = None;
        }

        if last_theme_poll.elapsed() >= Duration::from_secs(1) {
            last_theme_poll = Instant::now();
            maybe_refresh_theme(
                deps.config,
                deps.markdown,
                &mut active_theme_mode,
                &mut active_terminal_background,
                last_user_input.elapsed(),
            );
        }

        persist_drafts_if_enabled(state, deps.draft_store, &mut last_persisted_draft_signature)
            .await;

        terminal.draw(|frame| ui::render(frame, state, deps.markdown))?;

        if state.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(60))? {
            match event::read()? {
                Event::Key(key_event) => {
                    if key_event.kind == KeyEventKind::Press {
                        last_user_input = Instant::now();
                        handle_key_event(terminal, state, deps.context, deps.tx, key_event);
                        // Persist immediately after key-driven mutations (create/edit/delete).
                        persist_drafts_if_enabled(
                            state,
                            deps.draft_store,
                            &mut last_persisted_draft_signature,
                        )
                        .await;
                    }
                }
                Event::Mouse(mouse_event) => {
                    last_user_input = Instant::now();
                    handle_mouse_event(state, mouse_event);
                }
                _ => {}
            }
        }
    }

    Ok(())
}

async fn persist_drafts_if_enabled(
    state: &mut AppState,
    draft_store: Option<&DraftStore>,
    last_persisted_draft_signature: &mut Option<String>,
) {
    let Some(store) = draft_store else {
        return;
    };
    persist_review_drafts(state, store, last_persisted_draft_signature).await;
}

async fn process_worker_message(
    state: &mut AppState,
    message: WorkerMessage,
    markdown: &mut MarkdownRenderer,
    context: &DataContext,
    tx: &WorkerTx,
    draft_store: Option<&DraftStore>,
    last_persisted_draft_signature: &mut Option<String>,
) {
    match message {
        WorkerMessage::ViewerLoginLoaded { viewer_login } => {
            if let Some(login) = viewer_login {
                state.set_viewer_login(Some(login));
            }
        }
        WorkerMessage::PullRequestsLoaded {
            repository_label,
            viewer_login,
            result,
        } => {
            if state.route == Route::Search {
                state.end_operation();
            }
            state.set_repository_label(repository_label);
            if let Some(login) = viewer_login {
                state.set_viewer_login(Some(login));
            }

            match result {
                Ok(pulls) => {
                    state.error_message = None;
                    state.set_pull_requests(pulls);
                }
                Err(error) => {
                    state.error_message = Some(error);
                }
            }
        }
        WorkerMessage::PullRequestDataLoaded { pull, result } => {
            state.end_operation();

            match result {
                Ok(data) => {
                    state.error_message = None;
                    markdown.clear_diff_cache();
                    let load_warning = data.load_warning.clone();

                    if let Some(review) = state.review.as_mut()
                        && review.pull.number == pull.number
                        && review.pull.owner == pull.owner
                        && review.pull.repo == pull.repo
                    {
                        review.clear_diff();
                        let head_changed = review.set_data(data);
                        if let Some(warning) = load_warning {
                            state.error_message = Some(warning);
                        }
                        if head_changed && review.pending_review_comment_count() > 0 {
                            state.error_message = Some(format!(
                                "pull request changed upstream; {} pending inline comment(s) may now be outdated",
                                review.pending_review_comment_count()
                            ));
                        }
                        *last_persisted_draft_signature = None;
                        state.route = Route::Review;
                        return;
                    }

                    state.open_review(pull, data);
                    if let Some(warning) = load_warning {
                        state.error_message = Some(warning);
                    }
                    if let (Some(store), Some(review)) = (draft_store, state.review.as_mut()) {
                        match store.load_for_review(review).await {
                            Ok(LoadOutcome::Loaded {
                                pending_comments,
                                reply_drafts,
                            }) => {
                                review.apply_restored_drafts(pending_comments, reply_drafts);
                            }
                            Ok(LoadOutcome::None) => {}
                            Err(err) => {
                                state.error_message =
                                    Some(format!("failed to load saved draft: {err}"));
                            }
                        }
                    }
                    *last_persisted_draft_signature = None;
                }
                Err(error) => {
                    state.error_message = Some(error);
                }
            }
        }
        WorkerMessage::PullRequestResolved {
            repository_label,
            pull_number,
            result,
        } => {
            state.end_operation();
            state.set_repository_label(repository_label);

            match result {
                Ok(pull) => {
                    state.error_message = None;
                    state.begin_operation(format!("Loading pull request #{pull_number}"));
                    spawn_load_pull_request_data(tx.clone(), context.client.clone(), pull);
                }
                Err(error) => {
                    state.error_message = Some(error);
                    state.route = Route::Search;
                }
            }
        }
        WorkerMessage::PullRequestDiffLoaded { pull, result } => {
            state.end_operation();
            markdown.clear_diff_cache();

            let Some(review) = state.review.as_mut() else {
                return;
            };
            if review.pull.number != pull.number
                || review.pull.owner != pull.owner
                || review.pull.repo != pull.repo
            {
                return;
            }

            match result {
                Ok(diff) => {
                    state.error_message = None;
                    review.set_diff(diff);
                }
                Err(error) => {
                    review.set_diff_error(error);
                }
            }
        }
        WorkerMessage::MutationApplied {
            pull,
            clear_reply_root_key,
            clear_pending_review_comments,
            result,
        } => {
            state.end_operation();

            match result {
                Ok(data) => {
                    state.error_message = None;
                    markdown.clear_diff_cache();
                    if let Some(review) = state.review.as_mut()
                        && review.pull.number == pull.number
                        && review.pull.owner == pull.owner
                        && review.pull.repo == pull.repo
                    {
                        if let Some(root_key) = clear_reply_root_key {
                            review.clear_reply_draft(&root_key);
                        }
                        if clear_pending_review_comments {
                            review.clear_pending_review_comments();
                        }
                        review.set_data(data);
                        state.route = Route::Review;
                    }
                }
                Err(error) => {
                    state.error_message = Some(error);
                }
            }
        }
    }
}

async fn persist_review_drafts(
    state: &mut AppState,
    draft_store: &DraftStore,
    last_persisted_draft_signature: &mut Option<String>,
) {
    let Some(review) = state.review.as_ref() else {
        *last_persisted_draft_signature = None;
        return;
    };

    let has_pending = review.pending_review_comment_count() > 0;
    let has_replies = review
        .reply_drafts
        .values()
        .any(|body| !body.trim().is_empty());
    if !has_pending && !has_replies {
        if last_persisted_draft_signature.is_some() {
            if let Err(err) = draft_store.clear_for_review(review).await {
                state.error_message = Some(format!("failed to clear saved draft: {err}"));
            }
            *last_persisted_draft_signature = None;
        }
        return;
    }

    let signature = DraftStore::draft_signature(review);
    if last_persisted_draft_signature.as_deref() == Some(signature.as_str()) {
        return;
    }

    if let Err(err) = draft_store.save_for_review(review).await {
        state.error_message = Some(format!("failed to save draft: {err}"));
        return;
    }

    *last_persisted_draft_signature = Some(signature);
}

fn maybe_refresh_theme(
    config: &AppConfig,
    markdown: &mut MarkdownRenderer,
    active_mode: &mut ThemeMode,
    active_terminal_background: &mut Option<(u8, u8, u8)>,
    input_idle_for: Duration,
) {
    if config.theme_config.theme_preference != config::ThemePreference::Auto {
        return;
    }

    if input_idle_for < Duration::from_millis(250) {
        return;
    }

    let detected = config::detect_terminal_theme_sample_live(Duration::from_millis(20));
    let Some(detected) = detected else {
        return;
    };
    if detected.mode == *active_mode && detected.background_rgb == *active_terminal_background {
        return;
    }

    let runtime_theme = config
        .theme_config
        .resolve_runtime_theme_for_mode(detected.mode);
    theme::apply(
        runtime_theme.palette,
        runtime_theme.mode,
        detected.background_rgb,
    );
    if detected.mode != *active_mode {
        let _ = markdown.set_ocean_theme(runtime_theme.mode);
    }
    *active_mode = detected.mode;
    *active_terminal_background = detected.background_rgb;
}

fn handle_key_event(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    context: &DataContext,
    tx: &WorkerTx,
    key: KeyEvent,
) {
    match state.route {
        Route::Search => handle_search_key_event(state, context, tx, key),
        Route::Review => handle_review_key_event(terminal, state, context, tx, key),
    }
}

fn handle_mouse_event(state: &mut AppState, mouse: MouseEvent) {
    let Some(delta) = mouse_scroll_delta(mouse.kind) else {
        return;
    };

    match state.route {
        Route::Search => match delta {
            MouseScrollDelta::Up(lines) => {
                for _ in 0..lines {
                    state.search_move_up();
                }
            }
            MouseScrollDelta::Down(lines) => {
                for _ in 0..lines {
                    state.search_move_down();
                }
            }
        },
        Route::Review => {
            let Some(review) = state.review.as_mut() else {
                return;
            };

            match delta {
                MouseScrollDelta::Up(lines) => {
                    for _ in 0..lines {
                        review.scroll_preview_up();
                    }
                }
                MouseScrollDelta::Down(lines) => {
                    for _ in 0..lines {
                        review.scroll_preview_down();
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MouseScrollDelta {
    Up(u16),
    Down(u16),
}

fn mouse_scroll_delta(kind: MouseEventKind) -> Option<MouseScrollDelta> {
    const STEP: u16 = 3;

    match kind {
        MouseEventKind::ScrollUp => Some(MouseScrollDelta::Up(STEP)),
        MouseEventKind::ScrollDown => Some(MouseScrollDelta::Down(STEP)),
        _ => None,
    }
}

fn handle_search_input_edit_key(key: KeyEvent, input: &mut SearchInputState) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            input.unfocus();
            false
        }
        KeyCode::Backspace => {
            input.backspace();
            true
        }
        KeyCode::Char(ch) if !ch.is_control() => {
            input.push_char(ch);
            true
        }
        _ => false,
    }
}

fn handle_search_key_event(
    state: &mut AppState,
    context: &DataContext,
    tx: &WorkerTx,
    key: KeyEvent,
) {
    if state.is_search_focused() {
        if handle_search_input_edit_key(key, state.search_input_mut()) {
            state.recompute_search();
        }
        return;
    }

    match key.code {
        KeyCode::Char('q') => {
            state.should_quit = true;
        }
        KeyCode::Char('W') => {
            open_selected_pull_in_browser(state);
        }
        KeyCode::Char('s') => state.focus_search(),
        KeyCode::Char('u') => state.toggle_search_scope(),
        KeyCode::Char('i') => state.toggle_search_status_filter(),
        KeyCode::Char('o') => state.toggle_search_sort(),
        KeyCode::Down | KeyCode::Char('j') => state.search_move_down(),
        KeyCode::Up | KeyCode::Char('k') => state.search_move_up(),
        KeyCode::Enter => {
            if state.is_busy() {
                return;
            }

            let Some(pull) = state.selected_search_pull().cloned() else {
                return;
            };

            state.error_message = None;
            state.begin_operation(format!("Loading pull request #{}", pull.number));

            spawn_load_pull_request_data(tx.clone(), context.client.clone(), pull);
        }
        KeyCode::Char('R') => {
            if state.is_busy() {
                return;
            }

            state.error_message = None;
            state.begin_operation("Refreshing open pull requests");
            spawn_load_pull_requests(
                tx.clone(),
                context.client.clone(),
                context.owner.clone(),
                context.repo.clone(),
            );
        }
        _ => {}
    }
}

fn handle_review_key_event(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    context: &DataContext,
    tx: &WorkerTx,
    key: KeyEvent,
) {
    let active_tab = state
        .review
        .as_ref()
        .map(|review| review.active_tab())
        .unwrap_or(ReviewTab::Threads);
    let is_visual_mode = active_tab == ReviewTab::Diff
        && state
            .review
            .as_ref()
            .is_some_and(|review| review.has_diff_selection_anchor());

    if active_tab == ReviewTab::Threads
        && state
            .review
            .as_ref()
            .is_some_and(|review| review.is_thread_search_focused())
    {
        if let Some(review) = state.review.as_mut()
            && handle_search_input_edit_key(key, review.thread_search_input_mut())
        {
            review.refresh_thread_search_results();
        }
        return;
    }

    if active_tab == ReviewTab::Diff
        && state
            .review
            .as_ref()
            .is_some_and(|review| review.is_diff_search_focused())
    {
        if let Some(review) = state.review.as_mut()
            && handle_search_input_edit_key(key, review.diff_search_input_mut())
        {
            review.refresh_diff_search_results();
        }
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => {
                if let Some(review) = state.review.as_mut()
                    && (review.active_tab() == ReviewTab::Threads
                        || review.is_diff_content_focused())
                {
                    review.fast_scroll_down();
                    return;
                }
            }
            KeyCode::Char('u') => {
                if let Some(review) = state.review.as_mut()
                    && (review.active_tab() == ReviewTab::Threads
                        || review.is_diff_content_focused())
                {
                    review.fast_scroll_up();
                    return;
                }
            }
            _ => {}
        }
    }

    if active_tab == ReviewTab::Diff
        && key.code == KeyCode::Esc
        && let Some(review) = state.review.as_mut()
        && review.has_diff_selection_anchor()
    {
        review.clear_diff_selection_anchor();
        state.error_message = None;
        return;
    }

    match key.code {
        KeyCode::Char('q') => {
            state.should_quit = true;
        }
        KeyCode::Char('b') => {
            if is_visual_mode {
                return;
            }
            state.back_to_search();
            maybe_spawn_search_load(state, context, tx);
        }
        KeyCode::Esc => {
            state.back_to_search();
            maybe_spawn_search_load(state, context, tx);
        }
        KeyCode::Tab => {
            if is_visual_mode {
                return;
            }
            if let Some(review) = state.review.as_mut()
                && review.active_tab() == ReviewTab::Diff
            {
                if review.is_diff_content_focused() {
                    review.focus_diff_files();
                } else {
                    review.focus_diff_content();
                }
            }
        }
        KeyCode::BackTab => {
            if is_visual_mode {
                return;
            }
            if let Some(review) = state.review.as_mut() {
                review.next_tab();
                if review.active_tab() == ReviewTab::Diff {
                    review.focus_diff_files();
                }
            }
            load_active_diff_if_needed(state, tx);
        }
        KeyCode::Char('W') => {
            if active_tab == ReviewTab::Threads {
                open_selected_comment_in_browser(state);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(review) = state.review.as_mut() {
                review.move_down();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(review) = state.review.as_mut() {
                review.move_up();
            }
        }
        KeyCode::Char('o' | 'z') => {
            if let Some(review) = state.review.as_mut() {
                if active_tab == ReviewTab::Threads {
                    review.toggle_selected_thread_collapsed();
                } else if active_tab == ReviewTab::Diff {
                    review.toggle_selected_diff_directory_collapsed();
                }
            }
        }
        KeyCode::Char('n' | ']') => {
            if active_tab == ReviewTab::Diff
                && !is_visual_mode
                && let Some(review) = state.review.as_mut()
            {
                review.focus_diff_content();
                review.jump_next_hunk();
            }
        }
        KeyCode::Char('N' | '[') => {
            if active_tab == ReviewTab::Diff
                && !is_visual_mode
                && let Some(review) = state.review.as_mut()
            {
                review.focus_diff_content();
                review.jump_prev_hunk();
            }
        }
        KeyCode::Char('p') => {
            if active_tab == ReviewTab::Diff
                && !is_visual_mode
                && let Some(review) = state.review.as_mut()
            {
                review.focus_diff_content();
                if review.jump_next_pending_review_comment() {
                    state.error_message = None;
                } else {
                    state.error_message = Some("no pending inline comments".to_owned());
                }
            }
        }
        KeyCode::Char('P') => {
            if active_tab == ReviewTab::Diff
                && !is_visual_mode
                && let Some(review) = state.review.as_mut()
            {
                review.focus_diff_content();
                if review.jump_prev_pending_review_comment() {
                    state.error_message = None;
                } else {
                    state.error_message = Some("no pending inline comments".to_owned());
                }
            }
        }
        KeyCode::Char('f') => {
            if active_tab == ReviewTab::Threads
                && let Some(review) = state.review.as_mut()
            {
                review.toggle_resolved_filter();
            }
        }
        KeyCode::Char('R') => {
            if is_visual_mode {
                return;
            }
            if state.is_busy() {
                return;
            }

            let Some(pull) = state.review.as_ref().map(|review| review.pull.clone()) else {
                return;
            };
            state.error_message = None;
            state.begin_operation(format!("Refreshing pull request #{}", pull.number));
            spawn_load_pull_request_data(tx.clone(), context.client.clone(), pull);
        }
        KeyCode::Char('t') => {
            if active_tab != ReviewTab::Threads {
                return;
            }
            if state.is_busy() {
                return;
            }

            let Some(review) = state.review.as_ref() else {
                return;
            };
            let Some(thread_context) = review.selected_thread_context() else {
                state.error_message = Some("select a review thread row".to_owned());
                return;
            };

            let Some(thread_id) = thread_context.thread_id.clone() else {
                state.error_message =
                    Some("thread cannot be resolved (missing thread id)".to_owned());
                return;
            };

            let resolved = !thread_context.is_resolved;
            let operation = if resolved {
                "Resolving thread"
            } else {
                "Reopening thread"
            };

            execute_mutation(
                state,
                context,
                tx,
                review.pull.clone(),
                MutationRequest::SetReviewThreadResolved {
                    thread_id,
                    resolved,
                },
                None,
                operation,
            );
        }
        KeyCode::Char('e') => {
            if active_tab == ReviewTab::Threads {
                open_reply_editor(terminal, state);
            } else if active_tab == ReviewTab::Diff {
                open_pending_diff_comment_editor(terminal, state);
            }
        }
        KeyCode::Char('x') => {
            if active_tab == ReviewTab::Threads
                && let Some(review) = state.review.as_mut()
                && let Some(context) = review.selected_thread_context()
            {
                review.clear_reply_draft(&context.root_key);
            } else if active_tab == ReviewTab::Diff && !is_visual_mode {
                clear_selected_pending_diff_comment(state);
            }
        }
        KeyCode::Char('s') => match active_tab {
            ReviewTab::Threads => {
                let can_send_reply = state
                    .review
                    .as_ref()
                    .and_then(|review| review.selected_reply_draft())
                    .is_some_and(|draft| !draft.trim().is_empty());
                if can_send_reply {
                    send_selected_reply(state, context, tx);
                } else if let Some(review) = state.review.as_mut() {
                    review.focus_thread_search();
                }
            }
            ReviewTab::Diff => {
                if let Some(review) = state.review.as_mut()
                    && !review.is_diff_content_focused()
                    && !review.has_diff_selection_anchor()
                {
                    review.focus_diff_search();
                }
            }
        },
        KeyCode::Char('/') => {
            if active_tab == ReviewTab::Threads
                && let Some(review) = state.review.as_mut()
            {
                review.focus_thread_search();
            }
        }
        KeyCode::Char('C') => {
            if is_visual_mode {
                return;
            }
            open_submit_review_editor_and_submit(
                terminal,
                state,
                context,
                tx,
                ReviewSubmissionEvent::Comment,
            );
        }
        KeyCode::Char('A') => {
            if is_visual_mode {
                return;
            }
            open_submit_review_editor_and_submit(
                terminal,
                state,
                context,
                tx,
                ReviewSubmissionEvent::Approve,
            );
        }
        KeyCode::Char('X') => {
            if is_visual_mode {
                return;
            }
            open_submit_review_editor_and_submit(
                terminal,
                state,
                context,
                tx,
                ReviewSubmissionEvent::RequestChanges,
            );
        }
        KeyCode::Char('v') => {
            if active_tab == ReviewTab::Diff
                && let Some(review) = state.review.as_mut()
                && review.is_diff_content_focused()
            {
                if !review.has_diff_selection_anchor()
                    && review.selected_pending_review_comment().is_some()
                {
                    state.error_message = Some(
                        "cannot start visual selection on existing pending comment".to_owned(),
                    );
                    return;
                }
                let had_anchor = review.has_diff_selection_anchor();
                review.toggle_diff_selection_anchor();
                if !had_anchor && !review.has_diff_selection_anchor() {
                    state.error_message =
                        Some("visual selection only works on changed diff lines".to_owned());
                } else {
                    state.error_message = None;
                }
            }
        }
        _ => {}
    }
}

fn maybe_spawn_search_load(state: &mut AppState, context: &DataContext, tx: &WorkerTx) {
    if state.is_busy() || !state.pull_requests.is_empty() {
        return;
    }

    if state.route == Route::Search {
        state.begin_operation("Loading open pull requests");
        spawn_load_pull_requests(
            tx.clone(),
            context.client.clone(),
            context.owner.clone(),
            context.repo.clone(),
        );
    }
}

fn load_active_diff_if_needed(state: &mut AppState, tx: &WorkerTx) {
    if state.is_busy() {
        return;
    }

    let Some(review) = state.review.as_ref() else {
        return;
    };
    if review.active_tab() != ReviewTab::Diff {
        return;
    }
    if review.diff.is_some() || review.diff_error.is_some() {
        return;
    }

    let pull = review.pull.clone();
    let changed_files = review.data.changed_files.clone();

    state.error_message = None;
    state.begin_operation(format!("Loading diff for pull request #{}", pull.number));
    spawn_load_pull_request_diff(tx.clone(), pull, changed_files);
}

fn open_reply_editor(terminal: &mut Terminal<CrosstermBackend<Stdout>>, state: &mut AppState) {
    if state.is_busy() {
        return;
    }

    let Some(review) = state.review.as_mut() else {
        return;
    };
    let Some(context) = review.selected_thread_context() else {
        return;
    };

    let existing = review
        .reply_drafts
        .get(&context.root_key)
        .cloned()
        .unwrap_or_default();

    match editor::edit_with_system_editor(&existing, terminal) {
        Ok(Some(edited)) => {
            review.set_reply_draft(context.root_key, edited);
            state.error_message = None;
        }
        Ok(None) => {}
        Err(err) => {
            state.error_message = Some(format!("failed to open editor: {err}"));
        }
    }
}

fn open_pending_diff_comment_editor(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
) {
    if state.is_busy() {
        return;
    }

    let Some(review) = state.review.as_mut() else {
        return;
    };
    if !review.is_diff_content_focused() {
        state.error_message = Some("press [l] to focus diff lines before commenting".to_owned());
        return;
    }

    let existing = review
        .selected_pending_review_comment()
        .map(|comment| comment.body.clone())
        .unwrap_or_default();

    match editor::edit_with_system_editor(&existing, terminal) {
        Ok(Some(edited)) => match review.upsert_pending_review_comment_from_selection(edited) {
            Ok(()) => state.error_message = None,
            Err(message) => state.error_message = Some(message.to_owned()),
        },
        Ok(None) => {}
        Err(err) => {
            state.error_message = Some(format!("failed to open editor: {err}"));
        }
    }
}

fn clear_selected_pending_diff_comment(state: &mut AppState) {
    let Some(review) = state.review.as_mut() else {
        return;
    };
    if !review.is_diff_content_focused() {
        return;
    }

    if !review.remove_selected_pending_review_comment() {
        state.error_message = Some("no pending comment on selected line".to_owned());
        return;
    }
    state.error_message = None;
}

fn send_selected_reply(state: &mut AppState, context: &DataContext, tx: &WorkerTx) {
    if state.is_busy() {
        return;
    }

    let Some(review) = state.review.as_ref() else {
        return;
    };
    let Some(thread_context) = review.selected_thread_context() else {
        return;
    };

    let body = review
        .reply_drafts
        .get(&thread_context.root_key)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();

    if body.is_empty() {
        state.error_message = Some("reply is empty; press [e] to edit".to_owned());
        return;
    }

    execute_mutation(
        state,
        context,
        tx,
        review.pull.clone(),
        MutationRequest::ReplyToReviewComment {
            owner: review.pull.owner.clone(),
            repo: review.pull.repo.clone(),
            pull_number: review.pull.number,
            comment_id: thread_context.comment_id,
            body,
        },
        Some(thread_context.root_key),
        "Posting reply",
    );
}

fn open_submit_review_editor_and_submit(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    context: &DataContext,
    tx: &WorkerTx,
    event: ReviewSubmissionEvent,
) {
    if state.is_busy() {
        return;
    }

    let Some(review) = state.review.as_ref() else {
        return;
    };

    let pull = review.pull.clone();
    let pending_comments = review
        .pending_review_comments()
        .iter()
        .map(|comment| SubmitReviewComment {
            path: comment.path.clone(),
            body: comment.body.clone(),
            line: comment.line,
            side: pending_comment_side_to_octocrab(comment.side),
            start_line: comment.start_line,
            start_side: comment
                .start_line
                .map(|_| pending_comment_side_to_octocrab(comment.side)),
        })
        .collect::<Vec<_>>();
    let initial = String::new();

    let body = match editor::edit_with_system_editor(&initial, terminal) {
        Ok(Some(text)) => text.trim().to_owned(),
        Ok(None) => return,
        Err(err) => {
            state.error_message = Some(format!("failed to open editor: {err}"));
            return;
        }
    };

    if body.is_empty() && pending_comments.is_empty() {
        state.error_message = Some("review is empty; add text or stage inline comments".to_owned());
        return;
    }

    execute_mutation(
        state,
        context,
        tx,
        pull.clone(),
        MutationRequest::SubmitPullRequestReview {
            owner: pull.owner.clone(),
            repo: pull.repo.clone(),
            pull_number: pull.number,
            event: event.as_api_event().to_owned(),
            body,
            comments: pending_comments,
            expected_head_sha: pull.head_sha.clone(),
        },
        None,
        event.title(),
    );
}

fn pending_comment_side_to_octocrab(side: PendingReviewCommentSide) -> pulls::Side {
    match side {
        PendingReviewCommentSide::Left => pulls::Side::Left,
        PendingReviewCommentSide::Right => pulls::Side::Right,
    }
}

fn execute_mutation(
    state: &mut AppState,
    context: &DataContext,
    tx: &WorkerTx,
    pull: PullRequestSummary,
    mutation: MutationRequest,
    clear_reply_root_key: Option<String>,
    operation_label: impl Into<String>,
) {
    let operation_label = operation_label.into();
    state.error_message = None;

    state.begin_operation(operation_label);
    spawn_apply_mutation(
        tx.clone(),
        context.client.clone(),
        pull,
        mutation,
        clear_reply_root_key,
    );
}

fn open_selected_pull_in_browser(state: &mut AppState) {
    let Some(pull) = state.selected_search_pull() else {
        return;
    };

    let Some(url) = pull
        .html_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    else {
        state.error_message = Some("selected pull request has no web URL".to_owned());
        return;
    };

    match open_in_browser(url) {
        Ok(()) => state.error_message = None,
        Err(err) => {
            state.error_message = Some(format!("failed to open browser: {err}"));
        }
    }
}

fn open_selected_comment_in_browser(state: &mut AppState) {
    let Some(review) = state.review.as_ref() else {
        return;
    };
    let Some(node) = review.selected_node() else {
        return;
    };

    let url = match &node.comment {
        CommentRef::Review(comment) => comment.html_url.as_str(),
        CommentRef::Issue(comment) => comment.html_url.as_str(),
        CommentRef::ReviewSummary(review) => review.html_url.as_str(),
    };

    if url.trim().is_empty() {
        state.error_message = Some("selected comment has no web URL".to_owned());
        return;
    }

    match open_in_browser(url) {
        Ok(()) => state.error_message = None,
        Err(err) => {
            state.error_message = Some(format!("failed to open browser: {err}"));
        }
    }
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode")?;

    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(out);
    let terminal = Terminal::new(backend).context("failed to create ratatui terminal")?;

    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;

    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("failed to leave alternate screen")?;

    terminal.show_cursor().context("failed to show cursor")?;
    Ok(())
}
