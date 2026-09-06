//! CLI-specific launch behavior. Codex runs directly, avoiding cmd.exe's
//! interpretation of prompts and TOML overrides as shell commands.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentProvider {
    #[default]
    Claude,
    Codex,
}

impl AgentProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
        }
    }
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
}
