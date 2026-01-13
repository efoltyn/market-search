#![forbid(unsafe_code)]

pub mod adapter;
pub mod agent;
pub mod config;
pub mod contract;
pub mod diff;
pub mod executor;
pub mod finance;
pub mod memory;
pub mod metrics;
pub mod orchestrator;
pub mod persistence;
pub mod trajectory;
pub mod types;

mod error;

pub use adapter::{AdapterError, ChatStream, LlmAdapter};
pub use error::{Error, Result};
