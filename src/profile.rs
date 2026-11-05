//! Zones and frame marks for the Tracy profiler, built in only with
//! `--features tracy` (see CONTRIBUTING.md). Without the feature the macros
//! expand to nothing, so the default build pays nothing and links no C++.
//!
//! The macros check for a running client rather than assume one, so unit
//! tests built with the feature, which never start one, still run.

// Recording's marks aren't used on macOS, which doesn't record yet.
#![allow(unused_macros)]

/// A zone named `$name` from here to the end of the enclosing block.
#[cfg(feature = "tracy")]
macro_rules! zone {
    ($name:literal) => {
        let _zone = tracy_client::Client::running().map(|c| c.span(tracy_client::span_location!($name), 0));
    };
}
#[cfg(not(feature = "tracy"))]
macro_rules! zone {
    ($name:literal) => {};
}

/// One frame of the stream `$name`; Tracy shows its rate and its gaps.
#[cfg(feature = "tracy")]
macro_rules! frame_mark {
    ($name:literal) => {
        if let Some(c) = tracy_client::Client::running() {
            c.secondary_frame_mark(tracy_client::frame_name!($name));
        }
    };
}
#[cfg(not(feature = "tracy"))]
macro_rules! frame_mark {
    ($name:literal) => {};
}

/// Name the current thread in Tracy's timeline.
#[cfg(feature = "tracy")]
macro_rules! thread_name {
    ($name:literal) => {
        if let Some(c) = tracy_client::Client::running() {
            c.set_thread_name($name);
        }
    };
}
#[cfg(not(feature = "tracy"))]
macro_rules! thread_name {
    ($name:literal) => {};
}

/// Start the Tracy client: first thing in `main`, before any thread starts.
pub fn start() {
    #[cfg(feature = "tracy")]
    {
        tracy_client::Client::start();
        thread_name!("main");
    }
}
