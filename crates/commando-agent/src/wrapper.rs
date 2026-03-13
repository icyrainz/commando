//! Command wrapper for output optimization.
//!
//! Wraps commands with an external optimizer binary (e.g., RTK) to reduce
//! output size. Three strategies based on command complexity:
//!
//! 1. **Simple commands** (`docker ps`) → run as `<wrapper> docker ps`
//! 2. **Chain commands** (`cd /app && docker ps`) → run as `sh -c "cd /app && <wrapper> docker ps"`
//! 3. **Complex commands** (pipes, redirects, etc.) → run as `sh -c "..."` unchanged
//!
//! All analysis is quote-aware: metacharacters inside single or double quotes
//! are ignored.

use tokio::process::Command;

/// Shell metacharacters that require a real shell (excluding chain operators and |/&).
/// Pipe and background are checked separately to distinguish `|` from `||` and `&` from `&&`.
const SHELL_META: &[char] = &[
    '(', ')', '<', '>', '$', '`', '!', '{', '}', '*', '?', '[', ']', '~', '#', '\n',
];

/// Shell builtins that modify shell state and cannot be wrapped with RTK.
/// Covers sh, bash, and fish. Non-state-modifying builtins (echo, pwd, test, etc.)
/// are safe to wrap — RTK will pass through if it doesn't optimize them.
const SHELL_BUILTINS: &[&str] = &[
    "cd", "export", "source", ".", "set", "unset", "eval", "exec", "read", "alias", "unalias",
    "return", "exit", "shift", "trap", "local", "declare", "typeset", "readonly", "pushd", "popd",
];

/// Build the command to execute, wrapping with the given binary where possible.
///
/// `wrapper_bin` is the wrapper binary name (e.g., "rtk").
/// `shell` is the configured shell (e.g., "sh", "bash", "fish") for complex commands.
pub fn build_command(command: &str, shell: &str, wrapper_bin: &str) -> Command {
    if is_simple_command(command) {
        let mut c = Command::new(wrapper_bin);
        for arg in shell_words::split(command).unwrap_or_else(|_| vec![command.to_string()]) {
            c.arg(arg);
        }
        c
    } else if let Some(wrapped) = wrap_chain(command, wrapper_bin) {
        let mut c = Command::new(shell);
        c.arg("-c").arg(wrapped);
        c
    } else {
        let mut c = Command::new(shell);
        c.arg("-c").arg(command);
        c
    }
}

/// Iterate over a command string yielding (position, byte) only for characters
/// that are outside of single and double quotes.
fn unquoted_chars(command: &str) -> impl Iterator<Item = (usize, u8)> + '_ {
    let bytes = command.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            let b = bytes[i];
            let pos = i;
            i += 1;
            match b {
                b'\'' if !in_double => {
                    in_single = !in_single;
                    continue;
                }
                b'"' if !in_single => {
                    in_double = !in_double;
                    continue;
                }
                _ if in_single || in_double => continue,
                _ => return Some((pos, b)),
            }
        }
        None
    })
}

/// Returns true if `pattern` appears in `command` outside of quotes.
fn has_unquoted(command: &str, pattern: &str) -> bool {
    let pat = pattern.as_bytes();
    let bytes = command.as_bytes();
    for (pos, _) in unquoted_chars(command) {
        if pos + pat.len() <= bytes.len() && &bytes[pos..pos + pat.len()] == pat {
            return true;
        }
    }
    false
}

/// Returns true if the command contains any SHELL_META character outside of quotes.
fn has_shell_meta(command: &str) -> bool {
    for (_, b) in unquoted_chars(command) {
        for &meta in SHELL_META {
            if b == meta as u8 {
                return true;
            }
        }
    }
    false
}

/// Returns true if the command contains a pipe operator (single |, not ||) outside quotes.
fn has_pipe(command: &str) -> bool {
    let bytes = command.as_bytes();
    for (i, b) in unquoted_chars(command) {
        if b == b'|' {
            let is_or =
                (i + 1 < bytes.len() && bytes[i + 1] == b'|') || (i > 0 && bytes[i - 1] == b'|');
            if !is_or {
                return true;
            }
        }
    }
    false
}

/// Returns true if the command contains a background operator (single &, not &&) outside quotes.
fn has_background(command: &str) -> bool {
    let bytes = command.as_bytes();
    for (i, b) in unquoted_chars(command) {
        if b == b'&' {
            let is_and =
                (i + 1 < bytes.len() && bytes[i + 1] == b'&') || (i > 0 && bytes[i - 1] == b'&');
            if !is_and {
                return true;
            }
        }
    }
    false
}

/// Returns true if the command is a simple single command with no shell features.
fn is_simple_command(command: &str) -> bool {
    !has_shell_meta(command)
        && !has_unquoted(command, "&&")
        && !has_unquoted(command, "||")
        && !has_pipe(command)
        && !has_background(command)
}

/// Find unquoted chain operators (&&, ||, ;) and their positions.
fn find_chain_operators(command: &str) -> Vec<(usize, &'static str)> {
    let bytes = command.as_bytes();
    let mut ops = Vec::new();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (i, b) in unquoted_chars(command) {
        if seen.contains(&i) {
            continue;
        }
        if b == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
            ops.push((i, "&&"));
            seen.insert(i + 1);
        } else if b == b'|' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            ops.push((i, "||"));
            seen.insert(i + 1);
        } else if b == b';' {
            ops.push((i, ";"));
        }
    }
    ops
}

/// For commands chained with &&, ||, or ;, wrap each simple subcommand with rtk.
/// Returns None if the command has no chains or contains features that require a real shell.
fn wrap_chain(command: &str, wrapper_bin: &str) -> Option<String> {
    if has_shell_meta(command) || has_pipe(command) || has_background(command) {
        return None;
    }

    let ops = find_chain_operators(command);
    if ops.is_empty() {
        return None;
    }

    let mut result = String::new();
    let mut prev_end = 0;
    for (pos, op) in &ops {
        let part = command[prev_end..*pos].trim();
        if !part.is_empty() {
            let first_word = part.split_whitespace().next().unwrap_or("");
            if SHELL_BUILTINS.contains(&first_word) {
                result.push_str(part);
            } else {
                result.push_str(wrapper_bin);
                result.push(' ');
                result.push_str(part);
            }
        }
        result.push(' ');
        result.push_str(op);
        result.push(' ');
        prev_end = pos + op.len();
    }

    let last = command[prev_end..].trim();
    if !last.is_empty() {
        let first_word = last.split_whitespace().next().unwrap_or("");
        if SHELL_BUILTINS.contains(&first_word) {
            result.push_str(last);
        } else {
            result.push_str(wrapper_bin);
            result.push(' ');
            result.push_str(last);
        }
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command_detection() {
        assert!(is_simple_command("docker ps"));
        assert!(is_simple_command("ls -la /"));
        assert!(is_simple_command("cat /etc/os-release"));
        assert!(!is_simple_command("echo $HOME"));
        assert!(!is_simple_command("ls | grep foo"));
        assert!(!is_simple_command("cat file |grep x"));
        assert!(!is_simple_command("echo hello > file"));
        assert!(!is_simple_command("docker ps && df -h"));
        assert!(!is_simple_command("ls *.log"));
    }

    #[test]
    fn simple_command_edge_cases() {
        // Background job (&) is a shell feature
        assert!(!is_simple_command("sleep 10 &"));

        // Quoted metacharacters are safe — parser is quote-aware
        assert!(is_simple_command("echo \"hello > world\""));
        assert!(is_simple_command("echo '$HOME'"));

        // Unquoted metacharacters are still complex
        assert!(!is_simple_command("echo hello > file"));
        assert!(!is_simple_command("echo $HOME"));

        // || is not simple
        assert!(!is_simple_command("docker ps || true"));
    }

    /// Helper: wrap_chain with "rtk" as the wrapper binary.
    fn wrap_chain_rtk(command: &str) -> Option<String> {
        wrap_chain(command, "rtk")
    }

    #[test]
    fn chain_wrapping() {
        // Simple chains get wrapped
        assert_eq!(
            wrap_chain_rtk("docker ps && df -h"),
            Some("rtk docker ps && rtk df -h".to_string())
        );

        // Shell builtins are not wrapped
        assert_eq!(
            wrap_chain_rtk("cd /app && docker ps"),
            Some("cd /app && rtk docker ps".to_string())
        );

        // Semicolons work too
        assert_eq!(
            wrap_chain_rtk("docker ps ; ls -la"),
            Some("rtk docker ps ; rtk ls -la".to_string())
        );

        // Commands with pipes/redirects return None
        assert!(wrap_chain_rtk("docker ps | grep foo").is_none());
        assert!(wrap_chain_rtk("echo $HOME && ls").is_none());

        // Pipe is not confused with ||
        assert!(wrap_chain_rtk("cat file | grep x").is_none());
        assert!(wrap_chain_rtk("docker ps || echo fail").is_some());

        // No chains return None
        assert!(wrap_chain_rtk("docker ps").is_none());

        // || chains
        assert_eq!(
            wrap_chain_rtk("docker ps || echo \"failure\""),
            Some("rtk docker ps || rtk echo \"failure\"".to_string())
        );

        // Mixed && and ||
        assert_eq!(
            wrap_chain_rtk("docker ps && echo \"success\" || echo \"failure\""),
            Some("rtk docker ps && rtk echo \"success\" || rtk echo \"failure\"".to_string())
        );

        // Builtin with || fallback
        assert_eq!(
            wrap_chain_rtk("cd /app || exit 1"),
            Some("cd /app || exit 1".to_string())
        );

        // && inside quoted strings must NOT be split
        assert!(wrap_chain_rtk(r#"echo "foo && bar""#).is_none());
        assert!(wrap_chain_rtk("echo 'hello || world'").is_none());

        // Semicolons inside quotes must NOT be split
        assert!(wrap_chain_rtk(r#"echo "hello; world""#).is_none());

        // Real chain with quoted args should still work
        assert_eq!(
            wrap_chain_rtk(r#"docker ps && echo "done""#),
            Some(r#"rtk docker ps && rtk echo "done""#.to_string())
        );
    }
}
