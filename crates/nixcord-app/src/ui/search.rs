//! Message Search query parser, right-side results drawer, and local/demo search.
//! (Contract stub — full implementation & tests owned by `search-eng`.)

use super::Action;
use crate::app::NixApp;
use eframe::egui::Ui;
use nixcord_http::{MessageSearchQuery, MessageSearchResult};
use nixcord_model::{Message, Snowflake};
use nixcord_store::Store;

pub const PAGE_SIZE: u32 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Guild(Snowflake),
    Channel(Snowflake),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterToken {
    pub raw: String,
    pub resolved: Option<(Snowflake, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedSearch {
    pub query: MessageSearchQuery,
    pub terms: Vec<String>,
    pub author_filter: Option<FilterToken>,
    pub channel_filter: Option<FilterToken>,
}

#[derive(Debug, Default)]
pub struct SearchState {
    pub input: String,
    pub want_focus: bool,
    pub open: bool,
    pub loading: bool,
    pub request_id: u64,
    pub scope: Option<SearchScope>,
    pub submitted_raw: String,
    pub parsed: ParsedSearch,
    pub offset: u32,
    pub total_results: u32,
    pub hits: Vec<Message>,
    pub error: Option<String>,
}

impl SearchState {
    pub fn close(&mut self) {
        self.open = false;
        self.loading = false;
        self.hits.clear();
        self.error = None;
        self.total_results = 0;
        self.offset = 0;
        self.submitted_raw.clear();
        self.input.clear();
    }
}

pub fn parse_search_query(
    raw: &str,
    _store: &Store,
    _guild_id: Option<Snowflake>,
    offset: u32,
) -> ParsedSearch {
    let trimmed = raw.trim();
    ParsedSearch {
        query: MessageSearchQuery {
            content: (!trimmed.is_empty()).then(|| trimmed.to_string()),
            offset: (offset > 0).then_some(offset),
            ..Default::default()
        },
        terms: trimmed
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect(),
        author_filter: None,
        channel_filter: None,
    }
}

pub fn extract_hits(result: MessageSearchResult, _parsed: &ParsedSearch) -> (u32, Vec<Message>) {
    let hits = result
        .messages
        .into_iter()
        .filter_map(|group| {
            group
                .iter()
                .find(|m| m.hit)
                .cloned()
                .or_else(|| group.into_iter().next())
        })
        .take(PAGE_SIZE as usize)
        .collect();
    (result.total_results, hits)
}

pub fn local_search<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
    query: &MessageSearchQuery,
    terms: &[String],
    offset: u32,
) -> MessageSearchResult {
    let mut matched: Vec<Message> = messages
        .into_iter()
        .filter(|m| query.channel_id.is_none_or(|c| m.channel_id == c))
        .filter(|m| query.author_id.is_none_or(|a| m.author.id == a))
        .filter(|m| {
            let low = m.content.to_lowercase();
            terms.iter().all(|t| low.contains(t))
        })
        .cloned()
        .collect();
    matched.sort_by(|a, b| b.id.cmp(&a.id));
    let total_results = matched.len() as u32;
    let page = matched
        .into_iter()
        .skip(offset as usize)
        .take(PAGE_SIZE as usize)
        .map(|mut m| {
            m.hit = true;
            vec![m]
        })
        .collect();
    MessageSearchResult {
        total_results,
        messages: page,
    }
}

pub fn panel(ui: &mut Ui, _app: &NixApp, _actions: &mut Vec<Action>) {
    ui.label("Search results");
}
