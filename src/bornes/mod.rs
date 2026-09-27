//! The `bornes` (specs.md §3) — self-contained modules, each with its
//! own interception mechanism (or none at all, in `prosa`'s case, which is
//! just a pure function called by the others). `hook` is the fourth, added
//! 2026-09-25 for remote MCP servers and images (macOS/Linux/WSL only).

pub mod comandos;
pub mod hook;
pub mod mcp;
pub mod prompt;
pub mod prosa;
