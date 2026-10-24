//! Communication with GitHub: the authenticated REST and GraphQL client and the helpers that
//! shape its requests and read its responses.

mod client;

pub use client::{Client, encode_path, encode_segment, is_failed};
