//! The built-in tool implementations.
//!
//! Each tool is a zero-sized type that can be constructed with [`Default`] or
//! `new`, and is registered through [`crate::Registry`].

mod editing;
mod files;
mod search;
mod shell;
mod text;
mod web;

pub use editing::EditTool;
pub use files::ReadTool;
pub use files::WriteTool;
pub use search::GlobTool;
pub use search::GrepTool;
pub use shell::PwshTool;
pub use text::EchoTool;
pub use web::{
    Hit, TodoWriteTool, WebFetchTool, WebSearchTool, decode_entities, html_to_text, parse_brave,
    parse_ddg, parse_searxng, parse_tavily,
};
