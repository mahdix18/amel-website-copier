//! The pieces of the `mistercopy` tool that the desktop app reuses: crawl
//! and session options (saved with a copy, so either front end can resume
//! the other's copies), building requesters, and zip output.

pub mod archive;
pub mod options;
pub mod requesters;
