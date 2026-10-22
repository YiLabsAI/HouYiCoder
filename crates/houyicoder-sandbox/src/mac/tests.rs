//! The seatbelt fence suites, split by the property each covers: the live
//! narrow-write fence, the repo git-config read posture, command execution,
//! and the sandbox baseline itself.

mod exec;
mod git_config;
mod narrow_write;
mod seatbelt;
mod tmpdir;
