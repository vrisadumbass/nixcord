//! Discord-style Friends page (Online / All / Pending / Blocked / Add Friend).
//! (Contract stub — full implementation & tests owned by `friends-eng`.)

use super::Action;
use crate::app::NixApp;
use eframe::egui::Ui;
use nixcord_model::Snowflake;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FriendsTab {
    #[default]
    Online,
    All,
    Pending,
    Blocked,
    AddFriend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddFeedback {
    Ok(String),
    Err(String),
}

#[derive(Debug, Default)]
pub struct FriendsState {
    pub tab: FriendsTab,
    pub filter: String,
    pub add_input: String,
    pub add_busy: bool,
    pub add_feedback: Option<AddFeedback>,
    pub confirm_remove: Option<Snowflake>,
}

pub fn show(ui: &mut Ui, _app: &mut NixApp, _actions: &mut Vec<Action>) {
    ui.label("Friends");
}
