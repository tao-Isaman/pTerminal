//! CLI entry point for `pterminal resume --id <sid> [--dir <path>]`.
//!
//! A user (or, in practice, Claude Code's `SessionStart` hook re-invoking the
//! CLI on `claude --resume`) runs `pterminal resume` from a shell. We can't
//! just spawn a second GUI window pointed at the session: if pTerminal is
//! already running we want the *existing* window to pick up the resumed tab.
//! So the CLI writes a small "command file" describing the request and either
//! (a) finds a running `pterminal(.exe)` process and lets it notice the file
//! on its own (Task 2's startup/poll drain), or (b) falls through to a normal
//! GUI launch, which will drain the file itself once it starts.
//!
//! Command files live in `commands_dir()` (`<state base>/commands/`) as
//! `resume-<millis>-<pid>.json`, one per invocation — plain enough that a
//! background poll can glob `*.json`, parse, and delete without any locking.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A parsed `pterminal resume` invocation, ready to be written to a command
/// file for the (running or about-to-start) GUI process to pick up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResumeCmd {
    pub session_id: String,
    pub dir: PathBuf,
}

fn usage() -> String {
    "usage: pterminal resume --id <session-id> [--dir <path>]".to_string()
}

/// Session ids are UUID-shaped (hyphens + hex): an allowlist of ASCII
/// alphanumerics and `-` only. The id ends up embedded in a filename
/// (`write_command`) and later a resumed command line (Task 2/3), so only
/// safe characters are permitted. This allowlist is stricter than strictly
/// necessary but simple and safe.
fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Parses `std::env::args()`-shaped argv (`args[0]` is the program name).
///
/// `None` means "no subcommand" — the caller should launch the GUI normally.
/// `Some(Err(usage))` means bad input — the caller should print it and exit
/// non-zero. `Some(Ok(cmd))` is a fully-validated resume request.
pub fn parse_args(args: &[String]) -> Option<Result<ResumeCmd, String>> {
    let sub = args.get(1)?;
    if sub != "resume" {
        // Any other args[1] is a usage error, not "no subcommand": this
        // reserves the whole `pterminal <word>` namespace for future
        // subcommands instead of silently launching the GUI on a typo.
        return Some(Err(usage()));
    }

    let mut id: Option<String> = None;
    let mut dir: Option<PathBuf> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--id" => {
                let Some(v) = args.get(i + 1) else { return Some(Err(usage())) };
                id = Some(v.clone());
                i += 2;
            }
            "--dir" => {
                let Some(v) = args.get(i + 1) else { return Some(Err(usage())) };
                dir = Some(PathBuf::from(v));
                i += 2;
            }
            other => return Some(Err(format!("unknown flag: {other}\n{}", usage()))),
        }
    }

    let id = match id {
        Some(id) if is_valid_id(&id) => id,
        _ => return Some(Err(usage())),
    };

    let dir = match dir {
        Some(d) => {
            // Absolutize relative paths against the invoking process's cwd
            if d.is_absolute() {
                d
            } else {
                match std::env::current_dir() {
                    Ok(cwd) => cwd.join(d),
                    Err(e) => {
                        return Some(Err(format!("--dir is relative and could not determine current directory: {e}")));
                    }
                }
            }
        }
        None => match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                return Some(Err(format!("--dir not given and could not determine current directory: {e}")));
            }
        },
    };

    Some(Ok(ResumeCmd { session_id: id, dir }))
}

/// Where command files live for the real app: `<state base>/commands/`.
pub fn commands_dir() -> PathBuf {
    commands_dir_in(&crate::state::default_base())
}

fn commands_dir_in(base: &Path) -> PathBuf {
    base.join("commands")
}

/// Writes `cmd` as a new, uniquely-named JSON file in `commands_dir()` and
/// returns its path. Filenames only need to be unique per-process (a single
/// `pterminal resume` invocation writes exactly one file), so
/// millis-since-epoch + pid is enough without adding a lock or a counter.
pub fn write_command(cmd: &ResumeCmd) -> anyhow::Result<PathBuf> {
    write_command_in(cmd, &commands_dir())
}

fn write_command_in(cmd: &ResumeCmd, dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let path = dir.join(format!("resume-{millis}-{pid}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(cmd)?)?;
    Ok(path)
}

/// Drains every `*.json` file in `commands_dir()`: parses each as a
/// [`ResumeCmd`], deletes it regardless of whether it parsed, and returns the
/// successfully-parsed commands plus a count of malformed files. A missing
/// directory (nothing has ever written a command) is not an error — it's
/// just an empty drain.
///
/// Consumed by Task 2's startup/poll drain (`PtApp::drain_resume_commands`
/// in `app.rs`, called from both `PtApp::new` and `drain_events`).
pub fn read_and_delete_commands() -> (Vec<ResumeCmd>, usize) {
    read_and_delete_commands_in(&commands_dir())
}

fn read_and_delete_commands_in(dir: &Path) -> (Vec<ResumeCmd>, usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return (Vec::new(), 0), // missing dir => empty, not an error
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("json")))
        .collect();
    paths.sort();

    let mut cmds = Vec::new();
    let mut malformed = 0usize;
    for path in paths {
        match std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<ResumeCmd>(&t).ok()) {
            Some(cmd) => {
                // Re-validate the session id to ensure it meets security requirements
                if is_valid_id(&cmd.session_id) {
                    cmds.push(cmd);
                } else {
                    malformed += 1;
                }
            }
            None => malformed += 1,
        }
        let _ = std::fs::remove_file(&path);
    }
    (cmds, malformed)
}

/// True if another `pterminal(.exe)` process (any pid other than our own) is
/// currently running. Used by the CLI branch to decide whether to hand the
/// resume request off to a running instance (and exit) or fall through to
/// launching the GUI itself. Follows resources.rs's sysinfo usage pattern.
pub fn another_instance_running() -> bool {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let self_pid = std::process::id();
    sys.processes().iter().any(|(pid, proc)| {
        if pid.as_u32() == self_pid {
            return false;
        }
        let name = proc.name().to_string_lossy().to_lowercase();
        name == "pterminal" || name == "pterminal.exe"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    // ---- parse_args ----

    #[test]
    fn no_subcommand_returns_none() {
        let args = vec!["pterminal".to_string()];
        assert!(parse_args(&args).is_none());
    }

    #[test]
    fn resume_with_id_defaults_dir_to_cwd() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "abc123".into()];
        let cmd = parse_args(&args).expect("Some").expect("Ok");
        assert_eq!(cmd.session_id, "abc123");
        assert_eq!(cmd.dir, std::env::current_dir().unwrap());
    }

    #[test]
    fn resume_with_dir_uses_given_dir() {
        let args = vec![
            "pterminal".into(), "resume".into(),
            "--id".into(), "abc123".into(),
            "--dir".into(), "C:\\some\\path".into(),
        ];
        let cmd = parse_args(&args).expect("Some").expect("Ok");
        assert_eq!(cmd.dir, PathBuf::from("C:\\some\\path"));
    }

    #[test]
    fn missing_id_is_error() {
        let args = vec!["pterminal".into(), "resume".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn empty_id_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn id_with_forward_slash_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "a/b".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn id_with_backslash_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "a\\b".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn id_with_dot_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "a.b".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn id_with_dotdot_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "..".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn id_with_ampersand_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "abc&calc".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn id_with_space_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "abc calc".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn id_with_quote_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "abc\"def".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn valid_uuid_like_id_is_accepted() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into(), "550e8400-e29b-41d4-a716-446655440000".into()];
        let cmd = parse_args(&args).expect("Some").expect("Ok");
        assert_eq!(cmd.session_id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn relative_dir_is_absolutized() {
        let args = vec![
            "pterminal".into(), "resume".into(),
            "--id".into(), "abc123".into(),
            "--dir".into(), "subdir".into(),
        ];
        let cmd = parse_args(&args).expect("Some").expect("Ok");
        let expected = std::env::current_dir().unwrap().join("subdir");
        assert_eq!(cmd.dir, expected);
        assert!(cmd.dir.is_absolute());
    }

    #[test]
    fn absolute_dir_is_unchanged() {
        let args = vec![
            "pterminal".into(), "resume".into(),
            "--id".into(), "abc123".into(),
            "--dir".into(), "C:\\absolute\\path".into(),
        ];
        let cmd = parse_args(&args).expect("Some").expect("Ok");
        assert_eq!(cmd.dir, PathBuf::from("C:\\absolute\\path"));
    }

    #[test]
    fn unknown_subcommand_is_error() {
        let args = vec!["pterminal".into(), "bogus".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn unknown_flag_is_error() {
        let args = vec![
            "pterminal".into(), "resume".into(),
            "--id".into(), "abc".into(),
            "--weird".into(),
        ];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    #[test]
    fn dangling_id_flag_with_no_value_is_error() {
        let args = vec!["pterminal".into(), "resume".into(), "--id".into()];
        assert!(parse_args(&args).expect("Some").is_err());
    }

    // ---- commands_dir_in ----

    #[test]
    fn commands_dir_in_appends_commands_subdir() {
        let base = Path::new("C:\\fake\\base");
        assert_eq!(commands_dir_in(base), base.join("commands"));
    }

    // ---- write_command_in / read_and_delete_commands_in round trip ----

    #[test]
    fn write_then_drain_round_trip_counts_good_and_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commands");

        let cmd1 = ResumeCmd { session_id: "aaa111".into(), dir: PathBuf::from("C:\\repo1") };
        write_command_in(&cmd1, &dir).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let cmd2 = ResumeCmd { session_id: "bbb222".into(), dir: PathBuf::from("C:\\repo2") };
        write_command_in(&cmd2, &dir).unwrap();

        std::fs::write(dir.join("resume-malformed.json"), "not valid json {{{").unwrap();

        let (cmds, malformed) = read_and_delete_commands_in(&dir);
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        assert_eq!(malformed, 1);
        assert!(cmds.iter().any(|c| c.session_id == "aaa111"));
        assert!(cmds.iter().any(|c| c.session_id == "bbb222"));

        let remaining: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(remaining.is_empty(), "commands dir should be empty after drain");
    }

    #[test]
    fn read_and_delete_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("does-not-exist");
        let (cmds, malformed) = read_and_delete_commands_in(&dir);
        assert!(cmds.is_empty());
        assert_eq!(malformed, 0);
    }

    #[test]
    fn drain_rejects_invalid_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commands");

        // Write a valid command
        let cmd_valid = ResumeCmd { session_id: "valid-id-123".into(), dir: PathBuf::from("C:\\repo1") };
        write_command_in(&cmd_valid, &dir).unwrap();

        // Write a command with an invalid session id (contains `&`)
        let cmd_invalid = ResumeCmd { session_id: "invalid&id".into(), dir: PathBuf::from("C:\\repo2") };
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let pid = std::process::id();
        let invalid_path = dir.join(format!("resume-{millis}-{pid}-invalid.json"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&invalid_path, serde_json::to_string_pretty(&cmd_invalid).unwrap()).unwrap();

        let (cmds, malformed) = read_and_delete_commands_in(&dir);
        assert_eq!(cmds.len(), 1, "should have exactly one valid command");
        assert_eq!(cmds[0].session_id, "valid-id-123");
        assert_eq!(malformed, 1, "should count the invalid session id as malformed");
    }
}
