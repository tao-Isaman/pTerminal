//! `pterminal-core`: the egui-free half of pTerminal.
//!
//! Everything here is plain logic over the filesystem, child processes and
//! serde documents — the agent providers and their launch plans, hook-event
//! parsing, the persisted app state, message routing, the shared-context
//! files, git worktrees, shell history, the resource sampler, the update
//! check and the directory watcher. None of it knows what a window, a frame
//! or a `PtApp` is, and that is the point of the crate: it compiles and tests
//! without pulling in egui/eframe (`cargo test -p pterminal-core` is the fast
//! loop), and it cannot grow a UI dependency by accident because there is no
//! such dependency to reach for.
//!
//! **Dependency direction.** This crate depends on nothing else in the
//! workspace. `pterminal-term` (the embedded terminal) depends on it for
//! shell history, and the root `pterminal` binary depends on both. The
//! binary re-imports these modules at its crate root under their old names,
//! so app code still says `crate::hooks::…` — the module tree the app sees is
//! unchanged, only where it is compiled moved.
//!
//! **Test isolation.** Some tests here touch real, non-tempdir paths
//! (`hooks::events_file(id)`, `shared_ctx::orchestrator_dir()`). The
//! per-test tab-id spacing convention (90_2xx, 90_4xx, …) is kept per crate,
//! and `cargo test --workspace` runs each crate's test binary in turn, so
//! moving modules here added no new races. Tests that create/remove the
//! orchestrator directory on disk (guarded by the binary's
//! `test_support::lock_orchestrator_dir`) stay in the binary with that lock.

pub mod commands;
pub mod git;
pub mod history;
pub mod hooks;
pub mod messages;
pub mod orchestrator_model;
pub mod provider;
pub mod resources;
pub mod shared_ctx;
pub mod state;
pub mod update;
pub mod watcher;
