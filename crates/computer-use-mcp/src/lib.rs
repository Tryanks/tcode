//! In-process `tcode_computer_use` MCP server: pi-computer-use-style desktop
//! automation for every provider (accessibility-tree observation, state-scoped
//! refs, transactional actions). See `docs/computer-use.md` for the design.
//!
//! Served over streamable HTTP on `127.0.0.1:<random port>` with a bearer
//! token, mirroring `preview-mcp` / `orchestrate-mcp`. The macOS backend talks
//! to the AX C API, CGEvent, and `screencapture`; other platforms serve a stub
//! that reports the platform as unsupported.

pub mod backend;
pub mod outline;
pub mod permissions;
pub mod state;
pub mod tools;
