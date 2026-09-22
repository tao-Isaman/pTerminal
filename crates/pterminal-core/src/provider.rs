//! The provider abstraction: everything pTerminal does differently for
//! Claude Code versus Codex CLI lives behind [`AgentProvider`]'s methods, so
//! the rest of the app asks "does this tab's provider do X?" instead of
//! matching on the variant.
//!
//! Wave 2 (A3) pulled the `tab.provider == AgentProvider::…` conditionals
//! that had spread across `app.rs`, `term.rs`, `ui.rs`, `dialogs.rs` and
//! `resume.rs` behind this one type. Each of those branches was a place a
//! third provider would have to be threaded through by hand, and two of
//! them (`term::spawn_agent` and `Tab::respawn`) had already grown their
//! own copy of the Codex command-line build. Nothing here changes what
//! either provider does — the argv, the hook setup and every UI gate are
//! byte-for-byte what the inline branches produced (pinned by the tests at
//! the bottom, which were derived from the old code's output).
//!
//! Only three things outside this file may still name a variant: serde
//! (the on-disk `"claude"`/`"codex"` form), the two UI lists that offer
//! every provider (they iterate [`AgentProvider::ALL`]), and the
//! `pterminal resume --provider` parser ([`AgentProvider::from_cli_name`]).
//!
//! CLI-specific launch behavior: Codex runs directly, avoiding cmd.exe's
//! interpretation of prompts and TOML overrides as shell commands. Claude is
//! an npm shim on Windows and cannot be exec'd directly, so it runs through
//! `cmd.exe /c`.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::hooks::{self, AgentStatus, HookSetup};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentProvider {
    #[default]
    Claude,
    Codex,
}

/// The two coordination paths every agent tab carries so its provider can
/// rebuild the launch command line later (`Tab::respawn`) without the app
/// re-deriving them from the workspace. Provider-neutral on purpose: Claude
/// receives these through its per-tab settings file (`HookSetup`, named in
/// argv as `--settings`) rather than as bare arguments, but the tab keeps
/// them regardless of provider so the field never has to change shape when
/// a tab's provider does. `Default` (both `None`) for shells and dead
/// placeholders. Not persisted — `state::SavedTab` never carried them, and
/// a resumed tab recomputes them at spawn time exactly like a fresh one
/// (`dialogs::open_tab` / `PtApp::resume_saved_tabs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentContext {
    /// The main checkout's `.pterminal/shared.md` (`shared_ctx::ensure_shared_md`),
    /// always the MAIN repo's even for a worktree tab.
    pub shared_md: Option<PathBuf>,
    /// The per-agent README (`shared_ctx::write_agent_readme`, or the
    /// orchestrator variant). Its parent directory is what Codex gets as
    /// `--add-dir` so a worktree agent can still append messages.
    pub agent_readme: Option<PathBuf>,
}

/// The inputs a provider needs to build one launch. Borrowed rather than
/// owned because both callers (`term::spawn_agent`, `Tab::respawn`) already
/// hold everything and the struct lives for one call.
pub struct LaunchSpec<'a> {
    pub tab_id: u64,
    /// The tab title — Codex is told its own name through
    /// `developer_instructions` so messages addressed `to: "<title>"` make
    /// sense to it; Claude learns it through the hook settings instead.
    pub title: &'a str,
    /// Ignored when `resume_session` is `Some` (both providers): a resumed
    /// session continues its own conversation, it does not get a new prompt.
    pub prompt: &'a str,
    pub resume_session: Option<&'a str>,
    pub context: &'a AgentContext,
    /// `hooks::pterm_hook_exe()` — `None` means the helper isn't beside the
    /// running executable. Both providers name it in their status-capture
    /// command: Codex in its `notify` argv, Claude in every hook command of
    /// its per-tab settings file (`hooks::HookSetup::hook_helper`).
    pub hook_helper: Option<&'a Path>,
    /// The directory the child will run in. Not part of any argv — it is
    /// where [`AgentProvider::launch`] looks for the pre-A4 per-checkout hook
    /// entries to strip (`hooks::forget_checkout_hooks`).
    pub cwd: &'a Path,
}

/// A resolved launch: what `TabTerm::spawn` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
}

impl AgentProvider {
    /// Every provider, in menu order. The new-tab dialog and the
    /// orchestrator context menu iterate this instead of spelling the
    /// variants, so adding a provider here adds it to both lists.
    pub const ALL: [AgentProvider; 2] = [AgentProvider::Claude, AgentProvider::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
        }
    }

    /// The `pterminal resume --provider <name>` spelling — the same
    /// lowercase names serde writes to `state.json`, so the CLI and the
    /// saved state can never disagree about what a provider is called.
    pub fn from_cli_name(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The launch for a fresh spawn, a resume, or a restart: writes whatever
    /// per-tab file the provider's status capture needs, then returns the
    /// program and argv `TabTerm::spawn` runs. **A launch plan has the same
    /// shape for every provider (A4):** anything the child must be told is
    /// in argv, and anything argv points at is a per-tab file under
    /// `hooks::events_dir()` written here, first. Codex has always worked
    /// this way — its `notify` command rides in `-c notify=[...]` and names
    /// the tab's events file. Claude now does too: its hooks live in
    /// `hooks::tab_settings_file(tab_id)` (`hooks::write_tab_settings`),
    /// which [`Self::launch_args`] hands to Claude Code as
    /// `--settings <path>`. Nothing is written into the checkout any more;
    /// the old per-checkout `.claude/settings.local.json` routing is gone
    /// (its two failure modes are on `write_tab_settings`' doc comment),
    /// and the entries an older pTerminal left in `cwd`'s file are stripped
    /// on the way past (`hooks::forget_checkout_hooks`).
    ///
    /// Split as "side effects here, argv in the pure [`Self::launch_args`]"
    /// rather than one method returning extra args from the write, so the
    /// argv tests can pin the exact command line without touching disk and
    /// `spawn_agent` has one call to make instead of a prepare-then-launch
    /// pair it could get out of order. Fallible for Codex because its
    /// executable has to be found on PATH; for Claude because of the two
    /// file operations (`cmd.exe` itself is always there).
    pub fn launch(self, spec: &LaunchSpec) -> anyhow::Result<Launch> {
        let program = match self {
            Self::Claude => {
                hooks::forget_checkout_hooks(spec.cwd)?;
                hooks::write_tab_settings(&HookSetup {
                    tab_id: spec.tab_id,
                    shared_md: spec.context.shared_md.as_deref(),
                    agent_readme: spec.context.agent_readme.as_deref(),
                    agent_name: spec.title,
                    hook_helper: spec.hook_helper,
                })?;
                "cmd.exe".to_string()
            }
            Self::Codex => codex_executable()?.to_string_lossy().into_owned(),
        };
        Ok(Launch { program, args: self.launch_args(spec) })
    }

    /// The "Restart" launch (`Tab::respawn`): the same provider, tab id,
    /// title, coordination paths and cwd, but **no prompt and no `--resume`**
    /// — a restart is "bring the session back", not a re-run of whatever the
    /// tab originally opened with, and `respawn` documents why it
    /// deliberately doesn't resume the known `session_id` either. Expressed
    /// as an empty-prompt, no-resume [`Self::launch`] because that is exactly
    /// what both inline branches built before A3: `cmd /c claude` for Claude
    /// and `codex_args(.., "", None, ..)` for Codex. Going through `launch`
    /// also rewrites Claude's per-tab settings file — same content as at
    /// first spawn (same id, title and paths), so that is a no-op in effect,
    /// and it means a restart never depends on a `%TEMP%` file surviving.
    pub fn relaunch(
        self,
        tab_id: u64,
        title: &str,
        context: &AgentContext,
        hook_helper: Option<&Path>,
        cwd: &Path,
    ) -> anyhow::Result<Launch> {
        self.launch(&LaunchSpec { tab_id, title, prompt: "", resume_session: None, context, hook_helper, cwd })
    }

    /// Pure argv half of [`Self::launch`]: no disk access, only path
    /// arithmetic (`hooks::tab_settings_file`, `hooks::events_file` are
    /// functions of the tab id).
    pub fn launch_args(self, spec: &LaunchSpec) -> Vec<String> {
        match self {
            Self::Claude => claude_args(&hooks::tab_settings_file(spec.tab_id), spec.prompt, spec.resume_session),
            Self::Codex => codex_args(
                spec.tab_id,
                spec.title,
                spec.context.shared_md.as_deref(),
                spec.context.agent_readme.as_deref(),
                spec.prompt,
                spec.resume_session,
                spec.hook_helper,
            ),
        }
    }

    /// Whether the tab renders pTerminal's bottom input bar
    /// (`Tab::has_input_bar`, `ui::input_bar`). Codex uses its own native
    /// terminal input interface (README), so the bar is Claude-only.
    pub fn supports_input_bar(self) -> bool {
        match self {
            Self::Claude => true,
            Self::Codex => false,
        }
    }

    /// Whether the one-click context handoff (`PtApp::start_handoff`) is
    /// offered. It asks the session to write a file with the `Write` tool
    /// and completes on the next `Stop` hook event — both Claude Code
    /// mechanics; Codex has neither the hook nor a guaranteed tool by that
    /// name, so the button is hidden for it.
    pub fn supports_handoff(self) -> bool {
        match self {
            Self::Claude => true,
            Self::Codex => false,
        }
    }

    /// Whether the status bar shows the `ctx Nk/Mk` context-window readout.
    /// It is read from Claude Code's transcript JSONL (`Tab::transcript_path`,
    /// delivered by hook events) — Codex reports no transcript path and its
    /// token usage isn't displayed (README), so the readout is Claude-only.
    pub fn supports_context_readout(self) -> bool {
        match self {
            Self::Claude => true,
            Self::Codex => false,
        }
    }

    /// The status to force when the user presses Enter in this provider's
    /// terminal (the terminal itself, `ui.rs`, and the deferred
    /// message-submit Enter in `PtApp::drain_events`). Codex only reports
    /// turn COMPLETION through `notify`, never turn start, so without this a
    /// Codex tab would keep showing the previous turn's "idle" all the way
    /// through the next turn; resetting to `Unknown` on Enter is the
    /// honest reading. Claude's `UserPromptSubmit` hook already flips it to
    /// `Working`, so `None` — nothing to do.
    pub fn status_on_enter(self) -> Option<AgentStatus> {
        match self {
            Self::Claude => None,
            Self::Codex => Some(AgentStatus::Unknown),
        }
    }

    /// An extra line for the tab-strip hover card explaining what the
    /// status marker can and can't mean for this provider. Codex has no
    /// approval or subagent events, so its marker never shows "needs you"
    /// or a child row — worth saying where the user is already looking.
    pub fn hover_note(self) -> Option<&'static str> {
        match self {
            Self::Claude => None,
            Self::Codex => Some(
                "Status reflects turn completion; approval and subagent tracking are unavailable.",
            ),
        }
    }
}

/// Claude's argv, run through `cmd.exe` (it is an npm shim on Windows and
/// cannot be exec'd directly). Moved from `term::agent_args` in A3;
/// A4 added `--settings <settings>` (the tab's hook file, see
/// `hooks::write_tab_settings`) right after the program name, ahead of the
/// positional prompt. The rest is unchanged: `--resume <sid>` when
/// resuming, else the prompt as one argument with `"` stripped (cmd's
/// quoting would otherwise split it).
pub fn claude_args(settings: &Path, prompt: &str, resume: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "/c".into(),
        "claude".into(),
        "--settings".into(),
        settings.to_string_lossy().into_owned(),
    ];
    match resume {
        Some(sid) => {
            args.push("--resume".into());
            args.push(sid.into());
        }
        None => {
            let prompt = prompt.replace('"', "");
            if !prompt.is_empty() {
                args.push(prompt);
            }
        }
    }
    args
}

pub fn codex_executable() -> anyhow::Result<PathBuf> {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    find_codex(std::env::split_paths(&paths)).ok_or_else(|| anyhow::anyhow!(
        "Codex CLI was not found. Install Codex CLI and put codex.exe or its npm installation on PATH, then restart pTerminal."
    ))
}

fn find_codex(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    for dir in paths {
        let native = dir.join("codex.exe");
        if native.is_file() {
            return Some(native);
        }
        // Both the older bundled-vendor and current optional npm platform
        // package layouts. Never search outside the Codex package itself.
        if dir.join("codex.cmd").is_file() || dir.join("codex.ps1").is_file() {
            if let Some(exe) = find_native(&dir.join("node_modules/@openai/codex"), 9) {
                return Some(exe);
            }
        }
    }
    None
}

fn find_native(dir: &Path, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let ty = entry.file_type().ok()?;
        if ty.is_file() && entry.file_name() == "codex.exe" {
            return Some(entry.path());
        }
        if ty.is_dir() {
            if let Some(exe) = find_native(&entry.path(), depth - 1) {
                return Some(exe);
            }
        }
    }
    None
}

pub fn codex_args(
    id: u64,
    title: &str,
    shared: Option<&Path>,
    readme: Option<&Path>,
    prompt: &str,
    resume: Option<&str>,
    helper: Option<&Path>,
) -> Vec<String> {
    let mut args = vec!["--no-alt-screen".into()];
    let mut context = format!("You are the pTerminal agent named {title}.\n");
    for path in [shared, readme].into_iter().flatten() {
        context.push_str(&format!(
            "Read {} for shared context and coordination instructions.\n",
            path.display()
        ));
    }
    // Isolated agents need to append messages in the main checkout. Grant
    // only the coordination directory, not the whole main working tree.
    if let Some(dir) = readme.and_then(Path::parent) {
        args.extend(["--add-dir".into(), dir.to_string_lossy().into_owned()]);
    }
    args.extend([
        "-c".into(),
        format!(
            "developer_instructions={}",
            serde_json::to_string(&context).unwrap()
        ),
    ]);
    if let Some(helper) = helper {
        let command = serde_json::json!([helper, "CodexNotify", crate::hooks::events_file(id)]);
        args.extend(["-c".into(), format!("notify={command}")]);
    }
    if let Some(sid) = resume {
        args.extend(["resume".into(), sid.into()]);
    } else if !prompt.is_empty() {
        args.extend(["--".into(), prompt.into()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an installed Codex CLI; no login or model request"]
    fn installed_codex_accepts_launch_config() {
        use std::os::windows::process::CommandExt;
        let home = tempfile::tempdir().unwrap();
        let readme = home.path().join("README-agents.md");
        let mut args = codex_args(
            1,
            "agent \"quoted\" ไทย",
            Some(home.path()),
            Some(&readme),
            "",
            None,
            Some(Path::new("C:/Program Files/pTerminal/pterm_hook.exe")),
        );
        args.extend(["features".into(), "list".into()]);
        let output = std::process::Command::new(codex_executable().unwrap())
            .args(args)
            .env("CODEX_HOME", home.path())
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn codex_resume_and_prompt_are_separate() {
        let args = codex_args(1, "agent", None, None, "ignored", Some("abc-123"), None);
        assert_eq!(&args[args.len() - 2..], ["resume", "abc-123"]);
        assert!(!args.iter().any(|a| a == "ignored" || a == "--resume"));
        let prompt = "--flag \"quoted\" & echo %PATH% ไทย\nnext";
        let args = codex_args(1, "agent", None, None, prompt, None, None);
        assert_eq!(&args[args.len() - 2..], ["--", prompt]);
    }
    #[test]
    fn finds_native_and_npm_installations() {
        let root = tempfile::tempdir().unwrap();
        assert!(find_codex([root.path().into()]).is_none());
        std::fs::write(root.path().join("codex.cmd"), "").unwrap();
        let bin = root.path().join("node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("codex.exe"), "").unwrap();
        assert_eq!(
            find_codex([root.path().into()]),
            Some(bin.join("codex.exe"))
        );
        std::fs::write(root.path().join("codex.exe"), "").unwrap();
        assert_eq!(
            find_codex([root.path().into()]),
            Some(root.path().join("codex.exe"))
        );
    }

    // --- Task 3 (relocated from term.rs in A3): the Claude argv -----------
    //
    // A4: every Claude argv gained `--settings <per-tab file>` right after
    // `claude`. `SETTINGS` stands in for that path so the rest of each
    // expectation still reads as the literal capture it was.

    const SETTINGS: &str = "C:\\t\\pterminal\\tab-7.settings.json";

    #[test]
    fn claude_args_prompt_path_strips_quotes_unchanged() {
        let args = claude_args(Path::new(SETTINGS), "say \"hi\" now", None);
        assert_eq!(args, vec!["/c", "claude", "--settings", SETTINGS, "say hi now"]);
    }

    #[test]
    fn claude_args_empty_prompt_omits_trailing_arg() {
        let args = claude_args(Path::new(SETTINGS), "", None);
        assert_eq!(args, vec!["/c", "claude", "--settings", SETTINGS]);
    }

    #[test]
    fn claude_args_resume_ignores_prompt() {
        let args = claude_args(Path::new(SETTINGS), "ignored prompt entirely", Some("sess-123"));
        assert_eq!(args, vec!["/c", "claude", "--settings", SETTINGS, "--resume", "sess-123"]);
    }

    // --- A3: the abstraction ------------------------------------------------
    //
    // The argv expectations below are LITERAL captures of what the pre-A3
    // inline branches (`term::spawn_agent`, `Tab::respawn`) produced for the
    // same inputs, printed from the old code before it was moved. They pin
    // the refactor as byte-for-byte behaviour-preserving; if one of these
    // ever needs to change, that is a launch-behaviour change, not a tidy-up.
    //
    // A4 was such a change, made deliberately: the Claude expectations gained
    // `--settings <hooks::tab_settings_file(7)>` and nothing else. Codex's
    // are untouched.

    fn ctx() -> AgentContext {
        AgentContext {
            shared_md: Some(PathBuf::from("D:\\repo\\.pterminal\\shared.md")),
            agent_readme: Some(PathBuf::from("D:\\repo\\.pterminal\\README-agents.md")),
        }
    }

    const HELPER: &str = "C:\\Program Files\\pTerminal\\pterm_hook.exe";

    /// A `LaunchSpec` for tab 7 with every field the argv tests vary
    /// defaulted; `cwd` is a fresh temp dir so a Claude `launch` (which
    /// strips old hook entries there) never looks at a real checkout.
    fn spec7<'a>(context: &'a AgentContext, prompt: &'a str, resume: Option<&'a str>, cwd: &'a Path) -> LaunchSpec<'a> {
        LaunchSpec {
            tab_id: 7,
            title: "fix \"login\"",
            prompt,
            resume_session: resume,
            context,
            hook_helper: Some(Path::new(HELPER)),
            cwd,
        }
    }

    /// The Codex `notify` override, exactly as the old code serialised it:
    /// a JSON array with backslashes doubled. The events-file half depends
    /// on the machine's temp dir, so it is built from `hooks::events_file`
    /// with a plain `\`→`\\` doubling (not serde, so the test doesn't
    /// re-derive the value under test with the code under test).
    fn expected_notify(id: u64) -> String {
        let events = hooks::events_file(id).to_string_lossy().replace('\\', "\\\\");
        format!("notify=[\"C:\\\\Program Files\\\\pTerminal\\\\pterm_hook.exe\",\"CodexNotify\",\"{events}\"]")
    }

    /// The `--settings` value for tab 7: the machine's temp dir plus the
    /// fixed `pterminal\tab-7.settings.json` tail, checked here so the argv
    /// tests below can use `hooks::tab_settings_file` without re-deriving
    /// the value under test with the code under test.
    fn expected_settings(id: u64) -> String {
        let path = hooks::tab_settings_file(id).to_string_lossy().into_owned();
        let tail = format!("\\pterminal\\tab-{id}.settings.json");
        assert!(path.ends_with(&tail), "{path}");
        assert!(path.starts_with(&std::env::temp_dir().to_string_lossy().into_owned()), "{path}");
        path
    }

    const CODEX_DEV_INSTRUCTIONS: &str = "developer_instructions=\"You are the pTerminal agent named fix \\\"login\\\".\\nRead D:\\\\repo\\\\.pterminal\\\\shared.md for shared context and coordination instructions.\\nRead D:\\\\repo\\\\.pterminal\\\\README-agents.md for shared context and coordination instructions.\\n\"";

    #[test]
    fn claude_launch_fresh_with_prompt_matches_old_spawn_agent() {
        let dir = tempfile::tempdir().unwrap();
        let context = ctx();
        let launch = AgentProvider::Claude
            .launch(&spec7(&context, "do the thing \"quoted\"", None, dir.path()))
            .unwrap();
        assert_eq!(launch.program, "cmd.exe");
        assert_eq!(
            launch.args,
            vec!["/c".to_string(), "claude".into(), "--settings".into(), expected_settings(7), "do the thing quoted".into()]
        );
    }

    #[test]
    fn claude_launch_resume_matches_old_spawn_agent() {
        let dir = tempfile::tempdir().unwrap();
        let context = ctx();
        let launch = AgentProvider::Claude
            .launch(&spec7(&context, "ignored", Some("sess-abc"), dir.path()))
            .unwrap();
        assert_eq!(launch.program, "cmd.exe");
        assert_eq!(
            launch.args,
            vec!["/c".to_string(), "claude".into(), "--settings".into(), expected_settings(7), "--resume".into(), "sess-abc".into()]
        );
    }

    /// Respawn is the bare `cmd /c claude` the old `Tab::respawn` hard-coded
    /// (plus A4's `--settings`): no prompt, no `--resume`, whatever the tab
    /// knows.
    #[test]
    fn claude_relaunch_matches_old_respawn() {
        let dir = tempfile::tempdir().unwrap();
        let launch = AgentProvider::Claude
            .relaunch(7, "fix \"login\"", &ctx(), Some(Path::new(HELPER)), dir.path())
            .unwrap();
        assert_eq!(launch.program, "cmd.exe");
        assert_eq!(
            launch.args,
            vec!["/c".to_string(), "claude".into(), "--settings".into(), expected_settings(7)]
        );
    }

    #[test]
    fn codex_launch_fresh_with_prompt_matches_old_spawn_agent() {
        let context = ctx();
        assert_eq!(
            AgentProvider::Codex.launch_args(&spec7(&context, "do the thing \"quoted\"", None, Path::new("D:\\repo"))),
            vec![
                "--no-alt-screen".to_string(),
                "--add-dir".into(),
                "D:\\repo\\.pterminal".into(),
                "-c".into(),
                CODEX_DEV_INSTRUCTIONS.into(),
                "-c".into(),
                expected_notify(7),
                "--".into(),
                "do the thing \"quoted\"".into(),
            ]
        );
    }

    #[test]
    fn codex_launch_resume_matches_old_spawn_agent() {
        let context = ctx();
        assert_eq!(
            AgentProvider::Codex.launch_args(&spec7(&context, "ignored", Some("sess-abc"), Path::new("D:\\repo"))),
            vec![
                "--no-alt-screen".to_string(),
                "--add-dir".into(),
                "D:\\repo\\.pterminal".into(),
                "-c".into(),
                CODEX_DEV_INSTRUCTIONS.into(),
                "-c".into(),
                expected_notify(7),
                "resume".into(),
                "sess-abc".into(),
            ]
        );
    }

    /// Respawn keeps the stored coordination paths and the notify command
    /// (the old `respawn` passed `codex_shared`/`codex_readme` through) but
    /// drops the prompt and never resumes.
    #[test]
    fn codex_relaunch_args_match_old_respawn() {
        let context = ctx();
        assert_eq!(
            AgentProvider::Codex.launch_args(&spec7(&context, "", None, Path::new("D:\\repo"))),
            vec![
                "--no-alt-screen".to_string(),
                "--add-dir".into(),
                "D:\\repo\\.pterminal".into(),
                "-c".into(),
                CODEX_DEV_INSTRUCTIONS.into(),
                "-c".into(),
                expected_notify(7),
            ]
        );
    }

    /// A shell-like context (no paths, no helper): no `--add-dir`, no
    /// `notify`, just the instructions — the old `codex_args(7, "fix", None,
    /// None, "", None, None)` output.
    #[test]
    fn codex_launch_without_context_or_helper_matches_old_output() {
        let context = AgentContext::default();
        let spec = LaunchSpec {
            tab_id: 7,
            title: "fix",
            prompt: "",
            resume_session: None,
            context: &context,
            hook_helper: None,
            cwd: Path::new("D:\\repo"),
        };
        assert_eq!(
            AgentProvider::Codex.launch_args(&spec),
            vec![
                "--no-alt-screen".to_string(),
                "-c".into(),
                "developer_instructions=\"You are the pTerminal agent named fix.\\n\"".into(),
            ]
        );
    }

    /// `relaunch` is exactly `launch` with an empty prompt and no resume —
    /// for both providers, so the two can never drift apart again.
    #[test]
    fn relaunch_is_launch_with_no_prompt_and_no_resume() {
        let dir = tempfile::tempdir().unwrap();
        let context = ctx();
        for provider in AgentProvider::ALL {
            let spec = spec7(&context, "", None, dir.path());
            assert_eq!(
                provider.relaunch(7, "fix \"login\"", &context, Some(Path::new(HELPER)), dir.path()).unwrap().args,
                provider.launch_args(&spec),
                "{provider:?}"
            );
        }
    }

    #[test]
    fn capabilities_per_provider() {
        let c = AgentProvider::Claude;
        let x = AgentProvider::Codex;
        assert!(c.supports_input_bar() && !x.supports_input_bar());
        assert!(c.supports_handoff() && !x.supports_handoff());
        assert!(c.supports_context_readout() && !x.supports_context_readout());
        assert_eq!(c.status_on_enter(), None);
        assert_eq!(x.status_on_enter(), Some(AgentStatus::Unknown));
        assert_eq!(c.hover_note(), None);
        assert_eq!(
            x.hover_note(),
            Some("Status reflects turn completion; approval and subagent tracking are unavailable.")
        );
    }

    // --- A4: per-tab status capture -----------------------------------------

    /// A Claude launch writes the tab's settings file (the one argv names)
    /// with the launch's own title/paths/helper, and leaves the checkout
    /// alone: no `.claude/` appears in `cwd`.
    #[test]
    fn claude_launch_writes_the_per_tab_settings_it_names_and_not_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let context = ctx();
        let id = 990_107;
        let spec = LaunchSpec { tab_id: id, ..spec7(&context, "go", None, dir.path()) };
        let launch = AgentProvider::Claude.launch(&spec).unwrap();
        let named = PathBuf::from(&launch.args[3]);
        assert_eq!(named, hooks::tab_settings_file(id));
        let text = std::fs::read_to_string(&named).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let inject = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(
            inject,
            "cmd /c type \"D:\\repo\\.pterminal\\shared.md\" & echo. & type \"D:\\repo\\.pterminal\\README-agents.md\" & echo. & echo You are agent \"fix \"login\"\"."
        );
        let stop = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(stop, format!("\"{HELPER}\" Stop \"{}\"", hooks::events_file(id).display()));
        assert!(!dir.path().join(".claude").exists(), "a launch must not write into the checkout");
        let _ = std::fs::remove_file(named);
    }

    /// The migration runs on every Claude launch: an older pTerminal's
    /// entries in `cwd`'s `settings.local.json` are gone afterwards, the
    /// user's own settings are not. Codex never opens the file (a Codex
    /// launch needs the executable, so only `launch_args` is exercised —
    /// which touches no file at all).
    #[test]
    fn claude_launch_strips_stale_checkout_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let stale = format!(
            "{{\"hooks\":{{\"Stop\":[{{\"hooks\":[{{\"type\":\"command\",\"command\":{}}}]}}]}},\"permissions\":{{\"allow\":[\"Bash(npm:*)\"]}}}}",
            serde_json::to_string(&format!("\"{HELPER}\" Stop \"{}\"", hooks::events_file(3).display())).unwrap()
        );
        std::fs::write(claude.join("settings.local.json"), &stale).unwrap();
        let context = ctx();
        let id = 990_108;
        AgentProvider::Claude.launch(&LaunchSpec { tab_id: id, ..spec7(&context, "", None, dir.path()) }).unwrap();
        assert_eq!(
            std::fs::read_to_string(claude.join("settings.local.json")).unwrap(),
            "{\"permissions\":{\"allow\":[\"Bash(npm:*)\"]}}"
        );
        let _ = std::fs::remove_file(hooks::tab_settings_file(id));
    }

    #[test]
    fn all_lists_every_provider_once_and_cli_names_round_trip() {
        assert_eq!(AgentProvider::ALL, [AgentProvider::Claude, AgentProvider::Codex]);
        assert_eq!(AgentProvider::from_cli_name("claude"), Some(AgentProvider::Claude));
        assert_eq!(AgentProvider::from_cli_name("codex"), Some(AgentProvider::Codex));
        assert_eq!(AgentProvider::from_cli_name("Claude"), None, "case-sensitive like serde");
        assert_eq!(AgentProvider::from_cli_name(""), None);
        // The CLI spelling IS the serde spelling.
        for p in AgentProvider::ALL {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(AgentProvider::from_cli_name(json.trim_matches('"')), Some(p));
        }
    }
}
