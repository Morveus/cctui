//! Detector for `rm`/`rmdir` invocations that trip Claude Code's
//! `dangerousRemoval` safety check, which is `bypassImmune` — it prompts even
//! in `bypassPermissions`, where nothing can answer it.
//!
//! The parser is shallow (split on separators, find `rm`/`rmdir` heads,
//! classify targets) and errs towards refusing.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RmRisk {
    /// Target contains an expansion that may expand to nothing (`$D`, `${X}`).
    UnexpandedVariable,
    /// Relative glob in a command that also changes directory.
    RelativeGlobAfterCd,
    /// The filesystem root, a top-level system directory, or `~`.
    CriticalPath,
    /// The working directory or one of its ancestors.
    WorkingDirectory,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DangerousRm {
    pub target: String,
    pub risk: RmRisk,
}

impl RmRisk {
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::UnexpandedVariable => "an unexpanded shell variable that may expand to nothing",
            Self::RelativeGlobAfterCd => "a relative glob in a command that also changes directory",
            Self::CriticalPath => "a critical system path",
            Self::WorkingDirectory => "the working directory or one of its ancestors",
        }
    }
}

const CRITICAL_PATHS: &[&str] = &[
    "/", "~", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib32", "/lib64", "/media",
    "/mnt", "/opt", "/proc", "/root", "/run", "/sbin", "/srv", "/sys", "/tmp", "/usr", "/var",
];

/// Classify `command`, returning the first `rm`/`rmdir` target that would trip
/// the bypass-immune check. `cwd` is the session working directory when known.
#[must_use]
pub fn dangerous_removal(command: &str, cwd: Option<&str>) -> Option<DangerousRm> {
    let mut changes_dir = false;
    for segment in lex(command) {
        let mut head = 0;
        while head < segment.len()
            && (is_assignment(&segment[head].text)
                || matches!(
                    basename(&segment[head].text),
                    "sudo" | "env" | "command" | "nohup" | "time" | "exec"
                ))
        {
            head += 1;
        }
        let Some(first) = segment.get(head) else { continue };
        match basename(&first.text) {
            "cd" | "pushd" => {
                changes_dir = true;
                continue;
            }
            "rm" | "rmdir" => {}
            _ => continue,
        }
        let mut flags_done = false;
        for token in &segment[head + 1..] {
            if !flags_done && token.text == "--" {
                flags_done = true;
                continue;
            }
            if !flags_done && token.text.len() > 1 && token.text.starts_with('-') {
                continue;
            }
            if token.text.is_empty() {
                continue;
            }
            if let Some(risk) = classify(token, changes_dir, cwd) {
                return Some(DangerousRm { target: token.text.clone(), risk });
            }
        }
    }
    None
}

fn classify(token: &Token, changes_dir: bool, cwd: Option<&str>) -> Option<RmRisk> {
    if token.expansion == Expansion::Unprotected {
        return Some(RmRisk::UnexpandedVariable);
    }
    let trimmed = {
        let t = token.text.trim_end_matches('/');
        if t.is_empty() { "/" } else { t }
    };
    let base = {
        let b = trimmed.strip_suffix("/*").unwrap_or(trimmed);
        if b.is_empty() { "/" } else { b }
    };
    if CRITICAL_PATHS.contains(&base) {
        return Some(RmRisk::CriticalPath);
    }
    if base == "." || base == ".." {
        return Some(RmRisk::WorkingDirectory);
    }
    if base.starts_with('/')
        && let Some(cwd) = cwd
    {
        let cwd = cwd.trim_end_matches('/');
        if !cwd.is_empty() && (cwd == base || cwd.starts_with(&format!("{base}/"))) {
            return Some(RmRisk::WorkingDirectory);
        }
    }
    if changes_dir
        && token.glob
        && token.expansion == Expansion::None
        && !base.starts_with('/')
        && !base.starts_with('~')
    {
        return Some(RmRisk::RelativeGlobAfterCd);
    }
    None
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else { return false };
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Expansion {
    None,
    /// `${X:?}` — the shell aborts rather than expanding to nothing.
    Protected,
    Unprotected,
}

const fn merge(a: Expansion, b: Expansion) -> Expansion {
    match (a, b) {
        (Expansion::Unprotected, _) | (_, Expansion::Unprotected) => Expansion::Unprotected,
        (Expansion::Protected, _) | (_, Expansion::Protected) => Expansion::Protected,
        _ => Expansion::None,
    }
}

#[derive(Clone, Debug)]
struct Token {
    text: String,
    expansion: Expansion,
    glob: bool,
}

struct Builder {
    text: String,
    expansion: Expansion,
    glob: bool,
    started: bool,
}

impl Builder {
    const fn new() -> Self {
        Self { text: String::new(), expansion: Expansion::None, glob: false, started: false }
    }

    /// Returns the next index.
    fn push_expansion(&mut self, chars: &[char], at: usize) -> usize {
        let (literal, kind, consumed) = read_expansion(chars, at);
        self.text.push_str(&literal);
        self.expansion = merge(self.expansion, kind);
        at + consumed
    }

    /// Returns the next index. Nothing inside a single-quoted run expands.
    fn read_single_quoted(&mut self, chars: &[char], at: usize) -> usize {
        self.started = true;
        let mut i = at + 1;
        while i < chars.len() && chars[i] != '\'' {
            self.text.push(chars[i]);
            i += 1;
        }
        i + usize::from(i < chars.len())
    }

    /// Returns the next index. Expansions inside double quotes still count.
    fn read_double_quoted(&mut self, chars: &[char], at: usize) -> usize {
        self.started = true;
        let mut i = at + 1;
        while i < chars.len() && chars[i] != '"' {
            if chars[i] == '\\' && i + 1 < chars.len() {
                self.text.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if chars[i] == '$' {
                i = self.push_expansion(chars, i);
                continue;
            }
            self.text.push(chars[i]);
            i += 1;
        }
        i + usize::from(i < chars.len())
    }

    fn flush(&mut self, segment: &mut Vec<Token>) {
        if self.started {
            segment.push(Token {
                text: std::mem::take(&mut self.text),
                expansion: self.expansion,
                glob: self.glob,
            });
            self.expansion = Expansion::None;
            self.glob = false;
            self.started = false;
        }
    }
}

fn lex(command: &str) -> Vec<Vec<Token>> {
    let chars: Vec<char> = command.chars().collect();
    let len = chars.len();
    let mut segments: Vec<Vec<Token>> = Vec::new();
    let mut segment: Vec<Token> = Vec::new();
    let mut tok = Builder::new();
    let mut i = 0;
    while i < len {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                tok.flush(&mut segment);
                i += 1;
            }
            '\n' | ';' | '(' | ')' | '{' | '}' => {
                tok.flush(&mut segment);
                if !segment.is_empty() {
                    segments.push(std::mem::take(&mut segment));
                }
                i += 1;
            }
            '&' | '|' => {
                tok.flush(&mut segment);
                if !segment.is_empty() {
                    segments.push(std::mem::take(&mut segment));
                }
                i += 1;
                if i < len && chars[i] == c {
                    i += 1;
                }
            }
            '\'' => i = tok.read_single_quoted(&chars, i),
            '"' => i = tok.read_double_quoted(&chars, i),
            '\\' => {
                tok.started = true;
                if i + 1 < len {
                    tok.text.push(chars[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '$' => {
                tok.started = true;
                i = tok.push_expansion(&chars, i);
            }
            '*' | '?' | '[' => {
                tok.started = true;
                tok.glob = true;
                tok.text.push(c);
                i += 1;
            }
            _ => {
                tok.started = true;
                tok.text.push(c);
                i += 1;
            }
        }
    }
    tok.flush(&mut segment);
    if !segment.is_empty() {
        segments.push(segment);
    }
    segments
}

/// Read the expansion starting at `chars[at]` (`'$'`). Returns its literal
/// text, its kind, and how many characters were consumed.
fn read_expansion(chars: &[char], at: usize) -> (String, Expansion, usize) {
    let len = chars.len();
    if at + 1 >= len {
        return ("$".to_string(), Expansion::None, 1);
    }
    match chars[at + 1] {
        '{' => {
            let mut j = at + 2;
            while j < len && chars[j] != '}' {
                j += 1;
            }
            let inner: String = chars[at + 2..j.min(len)].iter().collect();
            let end = if j < len { j + 1 } else { len };
            let literal: String = chars[at..end].iter().collect();
            let kind =
                if inner.contains(":?") { Expansion::Protected } else { Expansion::Unprotected };
            (literal, kind, end - at)
        }
        '(' => {
            let mut depth = 0;
            let mut j = at + 1;
            while j < len {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            let literal: String = chars[at..j.min(len)].iter().collect();
            (literal, Expansion::Unprotected, j.min(len) - at)
        }
        c if c.is_ascii_alphanumeric() || c == '_' => {
            let mut j = at + 1;
            while j < len && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let literal: String = chars[at..j].iter().collect();
            (literal, Expansion::Unprotected, j - at)
        }
        c => (format!("${c}"), Expansion::Unprotected, 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk(command: &str) -> Option<RmRisk> {
        dangerous_removal(command, Some("/home/dorsk/Documents/repo")).map(|d| d.risk)
    }

    #[test]
    fn flags_bare_variable_target() {
        assert_eq!(risk("rm -f $D/*.json"), Some(RmRisk::UnexpandedVariable));
        assert_eq!(risk(r#"rm -rf "$X""#), Some(RmRisk::UnexpandedVariable));
        assert_eq!(risk("rm -rf ${X}"), Some(RmRisk::UnexpandedVariable));
        assert_eq!(risk("rm -rf ${X}/build"), Some(RmRisk::UnexpandedVariable));
        assert_eq!(risk("rm -rf $HOME/.cache/thing"), Some(RmRisk::UnexpandedVariable));
    }

    #[test]
    fn flags_the_reported_command() {
        let cmd = "D=~/.claude/artifacts/langfuse-cache/win; mkdir -p $D; rm -f $D/*.json; echo ok";
        let found = dangerous_removal(cmd, None).expect("dangerous");
        assert_eq!(found.risk, RmRisk::UnexpandedVariable);
        assert_eq!(found.target, "$D/*.json");
    }

    #[test]
    fn flags_command_substitution_target() {
        assert_eq!(risk("rm -rf $(cat list.txt)"), Some(RmRisk::UnexpandedVariable));
    }

    #[test]
    fn flags_relative_glob_after_cd() {
        assert_eq!(risk("cd /tmp/work && rm -f *.json"), Some(RmRisk::RelativeGlobAfterCd));
        assert_eq!(risk("cd build; rm -rf ./*"), Some(RmRisk::WorkingDirectory));
    }

    #[test]
    fn relative_glob_without_cd_is_fine() {
        assert_eq!(risk("rm -f *.json"), None);
    }

    #[test]
    fn flags_critical_paths() {
        for cmd in [
            "rm -rf /",
            "rm -rf /*",
            "rm -rf /usr",
            "rm -rf /etc/",
            "rm -rf /etc/*",
            "rm -rf ~",
            "rm -rf /home",
            "rmdir /var",
        ] {
            assert_eq!(risk(cmd), Some(RmRisk::CriticalPath), "{cmd}");
        }
    }

    #[test]
    fn flags_cwd_and_ancestors() {
        assert_eq!(risk("rm -rf /home/dorsk/Documents/repo"), Some(RmRisk::WorkingDirectory));
        assert_eq!(risk("rm -rf /home/dorsk/Documents/repo/"), Some(RmRisk::WorkingDirectory));
        assert_eq!(risk("rm -rf /home/dorsk/Documents"), Some(RmRisk::WorkingDirectory));
        assert_eq!(risk("rm -rf ."), Some(RmRisk::WorkingDirectory));
        assert_eq!(risk("rm -rf .."), Some(RmRisk::WorkingDirectory));
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn safe_forms_are_not_refused() {
        assert_eq!(risk("rm -f /home/dorsk/Documents/repo/tmp/out.json"), None);
        assert_eq!(risk(r#"rm -f -- "${D:?}"/*.json"#), None);
        assert_eq!(risk(r#"rm -rf "${BUILD_DIR:?}""#), None);
        assert_eq!(risk(r#"find "$D" -maxdepth 1 -name '*.json' -delete"#), None);
        assert_eq!(risk(r#"rm -f "/tmp/work/out.json""#), None);
        assert_eq!(risk("rm -f '$D/out.json'"), None);
        assert_eq!(risk("rm -rf /tmp/work/build"), None);
    }

    #[test]
    fn non_removal_commands_are_ignored() {
        assert_eq!(risk("echo rm -rf $D"), None);
        assert_eq!(risk("git status && cargo test"), None);
        assert_eq!(risk("cd /tmp && ls *.json"), None);
        assert_eq!(risk("grep -rn 'rm -rf /' ."), None);
    }

    #[test]
    fn sees_through_leading_assignments_and_sudo() {
        assert_eq!(risk("FOO=1 rm -rf $D"), Some(RmRisk::UnexpandedVariable));
        assert_eq!(risk("sudo rm -rf /usr"), Some(RmRisk::CriticalPath));
    }

    #[test]
    fn flags_are_not_mistaken_for_targets() {
        assert_eq!(risk("rm -rf -- /tmp/work/out"), None);
        assert_eq!(risk("rm --recursive --force /tmp/work/out"), None);
    }

    #[test]
    fn pipelines_and_newlines_split_segments() {
        assert_eq!(risk("ls | rm -rf $D"), Some(RmRisk::UnexpandedVariable));
        assert_eq!(risk("mkdir -p /tmp/x\nrm -rf /etc"), Some(RmRisk::CriticalPath));
    }

    #[test]
    fn missing_cwd_only_drops_the_cwd_rule() {
        assert_eq!(dangerous_removal("rm -rf /home/dorsk/Documents", None), None);
        assert_eq!(
            dangerous_removal("rm -rf $D", None).map(|d| d.risk),
            Some(RmRisk::UnexpandedVariable)
        );
    }
}
