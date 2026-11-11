//! The MisterCopy desktop app: a window that copies a site and shows the
//! crawl as it happens. The crawl itself runs on its own thread with its own
//! tokio runtime; the window only listens to its events.

pub mod app;
pub mod insight;
pub mod job;
pub mod projects;

pub use app::CopyWindow;
