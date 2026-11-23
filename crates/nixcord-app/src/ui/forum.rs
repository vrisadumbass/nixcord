//! Forum channel post board and Create Thread / New Post modal.
//! (Contract stub — full implementation & tests owned by `forum-eng`.)

use super::Action;
use crate::app::NixApp;
use eframe::egui::{self, Ui};
use nixcord_model::Snowflake;
use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct ForumState {
    pub forum_id: Option<Snowflake>,
    pub search: String,
    pub selected_tags: HashSet<Snowflake>,
}

#[derive(Debug, Clone)]
pub struct CreateThreadModal {
    pub parent_id: Snowflake,
    pub is_forum: bool,
    pub title: String,
    pub message: String,
    pub selected_tags: Vec<Snowflake>,
    pub busy: bool,
    pub error: Option<String>,
    pub focus_title: bool,
}

impl CreateThreadModal {
    pub fn new(parent_id: Snowflake, is_forum: bool) -> Self {
        Self {
            parent_id,
            is_forum,
            title: String::new(),
            message: String::new(),
            selected_tags: Vec::new(),
            busy: false,
            error: None,
            focus_title: true,
        }
    }
}

pub fn show(ui: &mut Ui, _app: &mut NixApp, _forum_id: Snowflake, _actions: &mut Vec<Action>) {
    ui.label("Forum");
}

pub fn create_modal(_ctx: &egui::Context, _app: &mut NixApp, _actions: &mut Vec<Action>) {}
