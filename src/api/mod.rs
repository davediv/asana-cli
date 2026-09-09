//! Thin Asana REST client: `client` does HTTP, pagination and error mapping;
//! `types` holds the serde models and the `opt_fields` lists each command asks for.

pub mod client;
pub mod types;

pub use client::{ApiError, Client};
pub use types::*;
