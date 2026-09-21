//! Picker behavior shared by Rebon's theme-chooser and message-rewind UIs.
//!
//! Two modules carry the behavior with production consumers:
//! [`theme_picker`] (the fixed theme list and the preview/save/cancel
//! transitions over it) and [`message_selector`] (the two-screen rewind
//! state machine). [`common`] holds the one option shape they have in
//! common.
//!
//! None of it renders, stores, or fetches anything: an outbound effect is
//! either returned as an action value the caller routes, or accepted as a
//! pre-built input, so a consumer owns every side effect.

#![deny(missing_docs)]

pub mod common;
pub mod message_selector;
pub mod theme_picker;
