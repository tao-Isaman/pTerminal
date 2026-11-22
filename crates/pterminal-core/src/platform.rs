//! Everything that differs between Windows and Linux (and other Unixes), in
//! one place. Callers ask "what runs a shell here" or "where is a Thai font"
//! and never spell `cmd.exe`, `powershell.exe` or `%WINDIR%\Fonts` themselves.
//!
//! Windows keeps exactly the behavior it had before the Linux port: npm CLIs
//! run through `cmd.exe /c` (they are `.cmd` shims there), shells are
//! `powershell.exe`, fonts come from `%WINDIR%\Fonts`.

use std::path::{Path, PathBuf};

/// `.exe` on Windows, empty elsewhere.
pub const EXE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

/// A program and the arguments that come before the caller's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

/// How to run an npm-installed CLI such as `claude`. On Windows npm installs
/// a `.cmd` shim, which cannot be exec'd directly, so it runs inside
/// `cmd.exe /c`; on Unix the npm bin is a real executable on `PATH`.
pub fn npm_cli(name: &str) -> Command {
    if cfg!(windows) {
        Command { program: "cmd.exe".into(), args: vec!["/c".into(), name.into()] }
    } else {
        Command { program: name.into(), args: vec![] }
    }
}

/// The interactive shell a plain shell tab runs: PowerShell on Windows, the
/// user's login shell (`$SHELL`, else `/bin/bash`) elsewhere.
pub fn shell() -> Command {
    if cfg!(windows) {
        Command { program: "powershell.exe".into(), args: vec![] }
    } else {
        shell_from_env(std::env::var_os("SHELL").map(PathBuf::from))
    }
}

fn shell_from_env(shell: Option<PathBuf>) -> Command {
    let program = shell
        .filter(|p| p.is_absolute())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/bin/bash".into());
    Command { program, args: vec![] }
}

/// A child that prints `words` (plain words, no shell metacharacters) and
/// exits with status 1: the placeholder tab shown when a saved tab cannot be
/// restored. Windows uses `cmd /c echo … & exit 1`, Unix `sh -c`.
pub fn fail_banner(words: &[&str]) -> Command {
    debug_assert!(words.iter().all(|w| w.chars().all(|c| c.is_alphanumeric() || " -.,:()".contains(c))));
    if cfg!(windows) {
        let mut args: Vec<String> = vec!["/c".into(), "echo".into()];
        args.extend(words.iter().map(|w| w.to_string()));
        args.extend(["&".into(), "exit".into(), "1".into()]);
        Command { program: "cmd.exe".into(), args }
    } else {
        Command { program: "/bin/sh".into(), args: vec!["-c".into(), format!("echo {}; exit 1", words.join(" "))] }
    }
}

/// The Codex CLI's native executable name.
pub fn codex_exe_name() -> String {
    format!("codex{EXE_SUFFIX}")
}

/// Directories that hold system and user fonts, most specific first.
pub fn font_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("WINDIR").map(|w| vec![PathBuf::from(w).join("Fonts")]).unwrap_or_default()
    } else {
        let mut dirs = Vec::new();
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
        }
        if let Some(data) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
            dirs.push(data.join("fonts"));
        }
        dirs.push("/usr/local/share/fonts".into());
        dirs.push("/usr/share/fonts".into());
        dirs
    }
}

/// The first of `names` (file names, no directory) found in any of `dirs`,
/// searched recursively a few levels deep (Linux keeps fonts in
/// `/usr/share/fonts/truetype/<family>/`). `names` order wins over `dirs`.
pub fn find_font_in(dirs: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|name| dirs.iter().find_map(|d| find_file(d, name, 4)))
}

/// [`find_font_in`] over [`font_dirs`].
pub fn find_font(names: &[&str]) -> Option<PathBuf> {
    find_font_in(&font_dirs(), names)
}

fn find_file(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    if depth == 0 {
        return None;
    }
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    subdirs.sort();
    subdirs.into_iter().find_map(|d| find_file(&d, name, depth - 1))
}

/// Font file names, best first, for each fallback role. Windows names are
/// the fonts every install ships; Linux names are the Noto/TLWG/DejaVu files
/// common desktop distributions install.
pub mod font_names {
    pub fn thai() -> &'static [&'static str] {
        if cfg!(windows) {
            &["LeelawUI.ttf", "tahoma.ttf"]
        } else {
            &["NotoSansThai-Regular.ttf", "NotoSansThaiUI-Regular.ttf", "NotoLoopedThai-Regular.ttf", "Loma.ttf", "Garuda.ttf"]
        }
    }
    pub fn symbols() -> &'static [&'static str] {
        if cfg!(windows) {
            &["seguisym.ttf"]
        } else {
            &["NotoSansSymbols2-Regular.ttf", "DejaVuSans.ttf"]
        }
    }
    /// A second symbol fallback, Linux only: Noto Sans Symbols 2 and DejaVu
    /// Sans both lack U+29C9 `⧉` (Claude Code's "open in editor" mark), which
    /// Noto Sans Math has. Segoe UI Symbol covers it on Windows.
    pub fn math() -> &'static [&'static str] {
        if cfg!(windows) { &[] } else { &["NotoSansMath-Regular.ttf"] }
    }
    /// Colour emoji: Segoe UI Emoji (COLR layers) on Windows, Noto Color Emoji
    /// (CBDT bitmaps) on Linux; `pterminal-term`'s emoji renderer draws both.
    pub fn emoji() -> &'static [&'static str] {
        if cfg!(windows) { &["seguiemj.ttf"] } else { &["NotoColorEmoji.ttf"] }
    }
}

/// The user's locale as a BCP-47-ish name (`ja-JP`), or empty. Windows asks
/// the OS; Unix reads `LC_ALL`, `LC_MESSAGES`, then `LANG` (`ja_JP.UTF-8`).
pub fn user_locale_name() -> String {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32;
        }
        let mut buf = [0u16; 85]; // LOCALE_NAME_MAX_LENGTH
        // SAFETY: `buf` is a valid, writable buffer of exactly the length passed.
        let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 1 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize - 1])
    }
    #[cfg(not(windows))]
    {
        let var = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
        posix_locale_to_bcp47(var.as_deref().unwrap_or(""))
    }
}

/// `ja_JP.UTF-8@x` → `ja-JP`; `C`/`POSIX` → empty.
pub fn posix_locale_to_bcp47(s: &str) -> String {
    let base = s.split(['.', '@']).next().unwrap_or("");
    if base.is_empty() || base == "C" || base == "POSIX" {
        return String::new();
    }
    base.replace('_', "-")
}

/// True for an environment variable that identifies the Claude Code *session*
/// which happened to launch pTerminal — its session id, messaging socket,
/// child/bridge ids, pid, and the `CLAUDECODE` marker. pTerminal is not a
/// Claude session; if it lets these reach an agent tab it spawns, that tab
/// looks like a CHILD of the launching session (it surfaces on that session's
/// Remote Control and messaging socket) instead of the independent root
/// session each bot must be. Matches the `CLAUDE_CODE_*` family plus the exact
/// names `CLAUDECODE` and `CLAUDE_PID`. Deliberately NOT `CLAUDE_CONFIG_DIR`
/// or other `CLAUDE_*` settings a child legitimately needs.
pub fn is_inherited_session_var(name: &str) -> bool {
    name.starts_with("CLAUDE_CODE_") || name == "CLAUDECODE" || name == "CLAUDE_PID"
}

/// Removes every [`is_inherited_session_var`] from THIS process's environment,
/// so nothing pTerminal spawns (agent tabs, shells) inherits the launching
/// Claude session's identity. A no-op when pTerminal was launched from a clean
/// environment (e.g. the autostart `.desktop`, or `env -u CLAUDE_CODE_*`).
///
/// # Safety / ordering
/// Must be called at the very top of `main`, before any thread starts and
/// before anything reads the environment: `std::env::remove_var` is `unsafe`
/// in edition 2024 because concurrent env access is a data race, and here the
/// program is still single-threaded.
pub fn scrub_inherited_session_env() {
    let doomed: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| is_inherited_session_var(k))
        .collect();
    for k in doomed {
        unsafe { std::env::remove_var(&k) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_session_vars_are_the_claude_session_family_only() {
        for v in [
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_BRIDGE_SESSION_ID",
            "CLAUDE_CODE_ANYTHING_ELSE",
            "CLAUDECODE",
            "CLAUDE_PID",
        ] {
            assert!(is_inherited_session_var(v), "{v} must be scrubbed");
        }
        for v in [
            "PATH", "HOME", "DISCORD_STATE_DIR",
            "CLAUDE_CONFIG_DIR",   // a child legitimately needs this
            "CLAUDECODEX",         // not the exact marker
            "CLAUDE_PIDFILE",      // not the exact name
            "MY_CLAUDE_CODE_VAR",  // prefix must be at the start
        ] {
            assert!(!is_inherited_session_var(v), "{v} must be kept");
        }
    }

    #[test]
    fn npm_cli_runs_through_cmd_only_on_windows() {
        let c = npm_cli("claude");
        if cfg!(windows) {
            assert_eq!(c, Command { program: "cmd.exe".into(), args: vec!["/c".into(), "claude".into()] });
        } else {
            assert_eq!(c, Command { program: "claude".into(), args: vec![] });
        }
    }

    // Unix-only: `/usr/bin/zsh` is not an absolute path on Windows (no drive).
    #[cfg(unix)]
    #[test]
    fn unix_shell_comes_from_shell_env_or_bash() {
        assert_eq!(shell_from_env(Some("/usr/bin/zsh".into())).program, "/usr/bin/zsh");
        assert_eq!(shell_from_env(Some("zsh".into())).program, "/bin/bash", "a relative $SHELL is not trusted");
        assert_eq!(shell_from_env(None).program, "/bin/bash");
    }

    #[test]
    fn fail_banner_exits_one_on_this_platform() {
        let c = fail_banner(&["pTerminal", "placeholder", "tab"]);
        if cfg!(windows) {
            assert_eq!(c.program, "cmd.exe");
            assert!(c.args.ends_with(&["&".into(), "exit".into(), "1".into()]));
        } else {
            assert_eq!(c.args, vec!["-c".to_string(), "echo pTerminal placeholder tab; exit 1".to_string()]);
        }
    }

    #[test]
    fn fonts_are_found_by_name_in_nested_dirs_in_name_order() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("truetype/noto");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("B.ttf"), b"b").unwrap();
        std::fs::write(root.path().join("A.ttf"), b"a").unwrap();
        let dirs = vec![root.path().to_path_buf()];
        assert_eq!(find_font_in(&dirs, &["B.ttf", "A.ttf"]), Some(deep.join("B.ttf")));
        assert_eq!(find_font_in(&dirs, &["missing.ttf"]), None);
    }

    #[test]
    fn posix_locales_map_to_bcp47() {
        assert_eq!(posix_locale_to_bcp47("ja_JP.UTF-8"), "ja-JP");
        assert_eq!(posix_locale_to_bcp47("th_TH"), "th-TH");
        assert_eq!(posix_locale_to_bcp47("C.UTF-8"), "");
        assert_eq!(posix_locale_to_bcp47(""), "");
    }
}
