use std::path::PathBuf;

const DEFAULT_SHELL: &str = "/bin/bash";

/// pTerminal delta: `scrolling_history` added — upstream always used
/// `alacritty_terminal`'s `term::Config::default()`, which put the scrollback
/// size out of reach of callers.
#[derive(Debug, Clone)]
pub struct BackendSettings {
    pub shell: String,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    pub scrolling_history: usize,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            shell: DEFAULT_SHELL.to_string(),
            args: vec![],
            working_directory: None,
            // Same value alacritty_terminal's `term::Config` defaults to.
            scrolling_history: 10_000,
        }
    }
}
