/// Pull the actual command output from `which <bin>` after shell-integration
/// noise. iTerm2 + zsh interactive mode prepends OSC escapes (ending in BEL
/// `\x07`) before stdout gets piped to us — everything before the final BEL
/// is preamble, not the path we want.
pub fn extract_path(raw: &str) -> String {
    let trimmed = match raw.rfind('\x07') {
        Some(idx) => &raw[idx + 1..],
        None => raw,
    };
    trimmed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::extract_path;

    #[test]
    fn plain_output() {
        assert_eq!(extract_path("/usr/local/bin/claude\n"), "/usr/local/bin/claude");
    }

    #[test]
    fn iterm_osc_prefix() {
        let raw = "\x1b]1337;RemoteHost=ryan@host\x07\x1b]1337;CurrentDir=/cwd\x07/Users/ryan/.local/bin/claude\n";
        assert_eq!(extract_path(raw), "/Users/ryan/.local/bin/claude");
    }

    #[test]
    fn no_bell_returns_trimmed_input() {
        assert_eq!(extract_path("  /opt/homebrew/bin/tmux  \n"), "/opt/homebrew/bin/tmux");
    }
}
