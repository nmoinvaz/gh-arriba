//! Communication with GitHub: the authenticated REST and GraphQL client and the helpers that
//! shape its requests and read its responses.

mod client;
pub mod dependabot;
pub mod json;
mod repo;

pub use client::{Client, encode_path, encode_segment, is_failed};
pub use repo::{PrRef, Repo, parse_pr_ref};
