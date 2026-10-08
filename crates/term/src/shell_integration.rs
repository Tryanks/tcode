#[cfg(unix)]
use std::io;

/// Only a completed shell prompt permits terminal cleanup. Input invalidates that
/// evidence until the shell reports another command and a subsequent prompt.
#[derive(Default)]
pub(crate) struct PromptEvidence {
    #[cfg(unix)]
    sequence: Vec<u8>,
    #[cfg(unix)]
    escape: bool,
    #[cfg(unix)]
    osc: bool,
    waiting_for_command: bool,
    at_prompt: Option<bool>,
}

impl PromptEvidence {
    pub fn input(&mut self) {
        self.at_prompt = Some(false);
        self.waiting_for_command = true;
    }

    pub fn at_prompt(&self) -> Option<bool> {
        self.at_prompt
    }

    #[cfg(unix)]
    pub fn output(&mut self, bytes: &[u8]) {
        // rio-vt retains OSC 133 A row annotations but discards B/C. Cleanup
        // needs the completed-prompt and command boundaries as well.
        for &byte in bytes {
            if self.osc {
                if byte == 7 || (self.escape && byte == b'\\') {
                    match self.sequence.as_slice() {
                        b"133;B" if !self.waiting_for_command => self.at_prompt = Some(true),
                        b"133;C" => {
                            self.waiting_for_command = false;
                            self.at_prompt = Some(false);
                        }
                        _ => {}
                    }
                    self.osc = false;
                    self.sequence.clear();
                } else if byte != 27 && self.sequence.len() < 128 {
                    self.sequence.push(byte);
                } else if self.sequence.len() >= 128 {
                    self.osc = false;
                    self.sequence.clear();
                }
            } else if self.escape && byte == b']' {
                self.osc = true;
            }
            self.escape = byte == 27;
        }
    }
}

#[cfg(unix)]
pub(crate) fn zsh_environment(
    program: &str,
    environment: &mut Vec<(String, String)>,
) -> io::Result<Option<tempfile::TempDir>> {
    if std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("zsh")
    {
        return Ok(None);
    }
    let supplied = |key: &str| {
        environment
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let original = supplied("ZDOTDIR")
        .or_else(|| std::env::var("ZDOTDIR").ok())
        .or_else(|| supplied("HOME"))
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_default();
    let directory = tempfile::Builder::new().prefix("tcode-shell-").tempdir()?;
    let forward = |file: &str| {
        format!(
            "export ZDOTDIR=\"$TCODE_ORIGINAL_ZDOTDIR\"\nif [[ -r \"$ZDOTDIR/{file}\" ]]; then source \"$ZDOTDIR/{file}\"; fi\nexport TCODE_ORIGINAL_ZDOTDIR=\"${{ZDOTDIR:-$HOME}}\"\nexport ZDOTDIR=\"$TCODE_INTEGRATION_DIR\"\n"
        )
    };
    std::fs::write(directory.path().join(".zshenv"), forward(".zshenv"))?;
    std::fs::write(directory.path().join(".zprofile"), forward(".zprofile"))?;
    std::fs::write(
        directory.path().join(".zshrc"),
        format!(
            "{}{}",
            forward(".zshrc"),
            r#"
export ZDOTDIR="$TCODE_ORIGINAL_ZDOTDIR"
autoload -Uz add-zsh-hook
_tcode_preexec() { printf '\e]133;C\a'; }
_tcode_precmd() {
    local prefix=$'%{\e]133;A\a%}' suffix=$'%{\e]133;B\a%}'
    PROMPT=${PROMPT#$prefix}
    PROMPT=${PROMPT%$suffix}
    PROMPT="$prefix$PROMPT$suffix"
}
add-zsh-hook preexec _tcode_preexec
add-zsh-hook precmd _tcode_precmd
"#
        ),
    )?;
    environment.extend([
        ("TCODE_ORIGINAL_ZDOTDIR".into(), original),
        (
            "TCODE_INTEGRATION_DIR".into(),
            directory.path().to_string_lossy().into_owned(),
        ),
        (
            "ZDOTDIR".into(),
            directory.path().to_string_lossy().into_owned(),
        ),
    ]);
    Ok(Some(directory))
}
