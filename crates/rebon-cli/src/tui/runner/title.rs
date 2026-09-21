use rebon_hooks::HookEffect;
use tokio::runtime::Handle;

use crate::session::title::{
    completed_session_title, completed_session_title_for, persist_session_title,
    session_title_update,
};
use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

pub(super) fn apply_session_title(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    title: String,
) {
    let title = title.trim().to_string();
    if title.is_empty() {
        return;
    }

    app.session_title = Some(title.clone());
    persist_session_title(session, &title);

    let publisher = session.engine_half.update_publisher.clone();
    let session_id = session.session_id.clone();
    handle.block_on(async move {
        publisher
            .publish_to(&session_id, session_title_update(title))
            .await;
    });
}

pub(super) fn mark_session_title_completed(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
) {
    let title = match app.session_title.as_deref() {
        Some(current) => completed_session_title(current),
        None => completed_session_title_for(session),
    };
    apply_session_title(app, session, handle, title);
}

pub(super) fn apply_session_title_effect(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    effect: &HookEffect,
) -> bool {
    match effect {
        HookEffect::SetSessionTitle { title } => {
            apply_session_title(app, session, handle, title.clone());
            true
        }
        HookEffect::MarkSessionComplete => {
            mark_session_title_completed(app, session, handle);
            true
        }
        _ => false,
    }
}
