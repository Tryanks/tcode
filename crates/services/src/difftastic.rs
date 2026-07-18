//! Pinned difftastic subprocess integration and defensive JSON row mapping.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

pub const DIFFTASTIC_VERSION: &str = "0.69.0";
const FILE_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralStatus {
    Unchanged,
    Changed,
    Created,
    Deleted,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralHighlight {
    Delimiter,
    #[default]
    Normal,
    String,
    Type,
    Comment,
    Keyword,
    TreeSitterError,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralSpan {
    pub start: usize,
    pub end: usize,
    pub highlight: StructuralHighlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralSide {
    /// One-based line number for the panel's gutter.
    pub line_number: u32,
    pub text: String,
    pub spans: Vec<StructuralSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralRow {
    pub lhs: Option<StructuralSide>,
    pub rhs: Option<StructuralSide>,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralFile {
    pub language: String,
    pub status: StructuralStatus,
    pub rows: Vec<StructuralRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuralError {
    MissingBinary,
    WrongVersion(String),
    Spawn(String),
    Timeout,
    Process(String),
    InvalidJson(String),
    Io(String),
}

#[derive(Debug, Default, Deserialize)]
struct JsonFile {
    #[serde(default)]
    aligned_lines: Vec<[Option<usize>; 2]>,
    #[serde(default)]
    chunks: Vec<Vec<JsonChangePair>>,
    #[serde(default)]
    language: String,
    #[serde(default)]
    status: StructuralStatus,
}

#[derive(Debug, Default, Deserialize)]
struct JsonChangePair {
    #[serde(default)]
    lhs: Option<JsonSide>,
    #[serde(default)]
    rhs: Option<JsonSide>,
}

#[derive(Debug, Default, Deserialize)]
struct JsonSide {
    #[serde(default)]
    line_number: usize,
    #[serde(default)]
    changes: Vec<JsonSpan>,
}

#[derive(Debug, Default, Deserialize)]
struct JsonSpan {
    #[serde(default)]
    start: usize,
    #[serde(default)]
    end: usize,
    #[serde(default)]
    highlight: StructuralHighlight,
}

/// Parse 0.69.0 JSON and attach full source text to its aligned line pairs.
/// Unknown JSON fields are intentionally ignored by serde.
pub fn parse_difft_json(
    json: &str,
    old_content: &str,
    new_content: &str,
) -> Result<StructuralFile, StructuralError> {
    let parsed: JsonFile = serde_json::from_str(json)
        .map_err(|error| StructuralError::InvalidJson(error.to_string()))?;
    let language = parsed.language.clone();
    let status = parsed.status;
    // `split` preserves the final empty line represented by a trailing newline;
    // difftastic includes that line in `aligned_lines`. Trailing `\r` is
    // stripped so CRLF inputs render and slice identically to LF inputs.
    fn strip_cr(line: &str) -> &str {
        line.strip_suffix('\r').unwrap_or(line)
    }
    let old_lines = old_content.split('\n').map(strip_cr).collect::<Vec<_>>();
    let new_lines = new_content.split('\n').map(strip_cr).collect::<Vec<_>>();
    let mut lhs_spans: HashMap<usize, Vec<StructuralSpan>> = HashMap::new();
    let mut rhs_spans: HashMap<usize, Vec<StructuralSpan>> = HashMap::new();
    let mut changed_lhs = HashSet::new();
    let mut changed_rhs = HashSet::new();
    for chunk in parsed.chunks {
        for pair in chunk {
            if let Some(side) = pair.lhs {
                changed_lhs.insert(side.line_number);
                lhs_spans
                    .entry(side.line_number)
                    .or_default()
                    .extend(side.changes.into_iter().filter_map(valid_span));
            }
            if let Some(side) = pair.rhs {
                changed_rhs.insert(side.line_number);
                rhs_spans
                    .entry(side.line_number)
                    .or_default()
                    .extend(side.changes.into_iter().filter_map(valid_span));
            }
        }
    }

    let mut rows = parsed
        .aligned_lines
        .into_iter()
        .filter_map(|[lhs, rhs]| {
            let lhs_side = lhs.and_then(|line| {
                old_lines.get(line).map(|text| StructuralSide {
                    line_number: line.saturating_add(1) as u32,
                    text: (*text).to_string(),
                    spans: lhs_spans.remove(&line).unwrap_or_default(),
                })
            });
            let rhs_side = rhs.and_then(|line| {
                new_lines.get(line).map(|text| StructuralSide {
                    line_number: line.saturating_add(1) as u32,
                    text: (*text).to_string(),
                    spans: rhs_spans.remove(&line).unwrap_or_default(),
                })
            });
            if lhs_side.is_none() && rhs_side.is_none() {
                return None;
            }
            Some(StructuralRow {
                changed: lhs.is_some_and(|line| changed_lhs.contains(&line))
                    || rhs.is_some_and(|line| changed_rhs.contains(&line))
                    || lhs.is_none()
                    || rhs.is_none(),
                lhs: lhs_side,
                rhs: rhs_side,
            })
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        match status {
            StructuralStatus::Created => {
                rows.extend(
                    new_lines
                        .iter()
                        .enumerate()
                        .map(|(line, text)| StructuralRow {
                            lhs: None,
                            rhs: Some(StructuralSide {
                                line_number: line.saturating_add(1) as u32,
                                text: (*text).to_string(),
                                spans: Vec::new(),
                            }),
                            changed: true,
                        }),
                );
            }
            StructuralStatus::Deleted => {
                rows.extend(
                    old_lines
                        .iter()
                        .enumerate()
                        .map(|(line, text)| StructuralRow {
                            lhs: Some(StructuralSide {
                                line_number: line.saturating_add(1) as u32,
                                text: (*text).to_string(),
                                spans: Vec::new(),
                            }),
                            rhs: None,
                            changed: true,
                        }),
                );
            }
            StructuralStatus::Unchanged => {
                rows.extend(old_lines.iter().zip(&new_lines).enumerate().map(
                    |(line, (old, new))| StructuralRow {
                        lhs: Some(StructuralSide {
                            line_number: line.saturating_add(1) as u32,
                            text: (*old).to_string(),
                            spans: Vec::new(),
                        }),
                        rhs: Some(StructuralSide {
                            line_number: line.saturating_add(1) as u32,
                            text: (*new).to_string(),
                            spans: Vec::new(),
                        }),
                        changed: false,
                    },
                ));
            }
            StructuralStatus::Changed | StructuralStatus::Unknown => {
                return Err(StructuralError::InvalidJson(
                    "changed difftastic result contained no aligned lines".into(),
                ));
            }
        }
    }
    Ok(StructuralFile {
        language,
        status,
        rows,
    })
}

fn valid_span(span: JsonSpan) -> Option<StructuralSpan> {
    (span.start < span.end).then_some(StructuralSpan {
        start: span.start,
        end: span.end,
        highlight: span.highlight,
    })
}

pub fn resolve_difftastic() -> Result<PathBuf, StructuralError> {
    resolve_candidates(difftastic_candidates())
}

fn difftastic_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = agent::find_on_path(if cfg!(windows) { "difft.exe" } else { "difft" }) {
        candidates.push(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        #[cfg(target_os = "macos")]
        if let Some(contents) = exe.parent().and_then(Path::parent) {
            candidates.push(contents.join("Resources/bin/difft"));
        }
        #[cfg(target_os = "linux")]
        if let Some(prefix) = exe.parent().and_then(Path::parent) {
            candidates.push(prefix.join("libexec/tcode/difft"));
        }
        #[cfg(windows)]
        if let Some(directory) = exe.parent() {
            candidates.push(directory.join("difft.exe"));
        }
    }
    candidates
}

fn resolve_candidates(candidates: Vec<PathBuf>) -> Result<PathBuf, StructuralError> {
    let mut wrong_version = None;
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        match binary_version(&candidate) {
            Ok(version) if version == DIFFTASTIC_VERSION => return Ok(candidate),
            Ok(version) => wrong_version = Some(version),
            Err(_) => continue,
        }
    }
    Err(wrong_version
        .map(StructuralError::WrongVersion)
        .unwrap_or(StructuralError::MissingBinary))
}

fn binary_version(binary: &Path) -> Result<String, StructuralError> {
    let output = crate::process::command(binary)
        .arg("--version")
        .output()
        .map_err(|error| StructuralError::Spawn(error.to_string()))?;
    if !output.status.success() {
        return Err(StructuralError::Process(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Difftastic "))
        .map(str::trim)
        .map(str::to_string)
        .ok_or_else(|| StructuralError::WrongVersion(stdout.trim().to_string()))
}

pub fn run_structural_diff(
    path: &Path,
    old_content: &str,
    new_content: &str,
) -> Result<StructuralFile, StructuralError> {
    let binary = resolve_difftastic()?;
    run_structural_diff_with_binary(&binary, path, old_content, new_content)
}

/// Explicit-binary entry point used by fixture verification and fallback tests.
pub fn run_structural_diff_with_binary(
    binary: &Path,
    path: &Path,
    old_content: &str,
    new_content: &str,
) -> Result<StructuralFile, StructuralError> {
    run_structural_diff_with_timeout(binary, path, old_content, new_content, FILE_TIMEOUT)
}

fn run_structural_diff_with_timeout(
    binary: &Path,
    path: &Path,
    old_content: &str,
    new_content: &str,
    timeout: Duration,
) -> Result<StructuralFile, StructuralError> {
    let version = binary_version(binary)?;
    if version != DIFFTASTIC_VERSION {
        return Err(StructuralError::WrongVersion(version));
    }
    let cache_key = content_hash(path, old_content, new_content, &version);
    if let Some(cached) = structural_cache().lock().unwrap().get(&cache_key).cloned() {
        return Ok(cached);
    }

    let temp = TempFiles::new(path, old_content, new_content)?;
    let mut child = crate::process::command(binary)
        .env("DFT_UNSTABLE", "yes")
        .args(["--display=json", "--color=never"])
        .arg(&temp.old)
        .arg(&temp.new)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| StructuralError::Spawn(error.to_string()))?;
    let stdout = child.stdout.take().expect("piped difftastic stdout");
    let stderr = child.stderr.take().expect("piped difftastic stderr");
    let stdout_reader = std::thread::spawn(move || read_pipe(stdout));
    let stderr_reader = std::thread::spawn(move || read_pipe(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(15));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(StructuralError::Timeout);
            }
            Err(error) => return Err(StructuralError::Io(error.to_string())),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| StructuralError::Io("difftastic stdout reader panicked".into()))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| StructuralError::Io("difftastic stderr reader panicked".into()))??;
    if !status.success() {
        return Err(StructuralError::Process(
            String::from_utf8_lossy(&stderr).trim().to_string(),
        ));
    }
    let json = String::from_utf8(stdout)
        .map_err(|error| StructuralError::InvalidJson(error.to_string()))?;
    let result = parse_difft_json(&json, old_content, new_content)?;
    structural_cache()
        .lock()
        .unwrap()
        .insert(cache_key, result.clone());
    Ok(result)
}

fn read_pipe(mut pipe: impl Read) -> Result<Vec<u8>, StructuralError> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)
        .map_err(|error| StructuralError::Io(error.to_string()))?;
    Ok(bytes)
}

fn structural_cache() -> &'static Mutex<HashMap<String, StructuralFile>> {
    static CACHE: OnceLock<Mutex<HashMap<String, StructuralFile>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn content_hash(path: &Path, old: &str, new: &str, version: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"structural\0");
    hasher.update(version.as_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(old.as_bytes());
    hasher.update(b"\0");
    hasher.update(new.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct TempFiles {
    directory: PathBuf,
    old: PathBuf,
    new: PathBuf,
}

impl TempFiles {
    fn new(path: &Path, old_content: &str, new_content: &str) -> Result<Self, StructuralError> {
        let directory =
            std::env::temp_dir().join(format!("tcode-difftastic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).map_err(|error| StructuralError::Io(error.to_string()))?;
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("txt");
        let old = directory.join(format!("old.{extension}"));
        let new = directory.join(format!("new.{extension}"));
        std::fs::write(&old, old_content)
            .and_then(|_| std::fs::write(&new, new_content))
            .map_err(|error| StructuralError::Io(error.to_string()))?;
        Ok(Self {
            directory,
            old,
            new,
        })
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = include_str!("../tests/fixtures/difftastic_0_69_0_before.rs");
    const AFTER: &str = include_str!("../tests/fixtures/difftastic_0_69_0_after.rs");
    const JSON: &str = include_str!("../tests/fixtures/difftastic_0_69_0_rust.json");

    #[test]
    fn recorded_json_maps_aligned_rows_and_novel_spans() {
        let file = parse_difft_json(JSON, BEFORE, AFTER).unwrap();
        assert_eq!(file.status, StructuralStatus::Changed);
        assert_eq!(file.language, "Rust");
        assert_eq!(file.rows.len(), 8);
        assert!(!file.rows[2].changed);
        assert!(file.rows[0].changed);
        assert_eq!(file.rows[0].lhs.as_ref().unwrap().line_number, 1);
        assert_eq!(
            file.rows[0].rhs.as_ref().unwrap().text,
            "fn greet(person: &str) -> String {"
        );
        assert_eq!(
            file.rows[1].rhs.as_ref().unwrap().spans[1].highlight,
            StructuralHighlight::String
        );
    }

    #[test]
    fn parser_ignores_unknown_fields_and_highlight_kinds() {
        let json = r#"{
            "aligned_lines": [[0, 0]],
            "chunks": [[{"lhs":{"line_number":0,"changes":[
                {"start":0,"end":1,"highlight":"future_kind","extra":true}
            ]}}]],
            "language":"Text", "status":"future_status", "extra":42
        }"#;
        let file = parse_difft_json(json, "a\n", "b\n").unwrap();
        assert_eq!(file.status, StructuralStatus::Unknown);
        assert_eq!(
            file.rows[0].lhs.as_ref().unwrap().spans[0].highlight,
            StructuralHighlight::Unknown
        );
    }

    #[test]
    fn invalid_and_out_of_range_rows_are_dropped_defensively() {
        assert!(matches!(
            parse_difft_json("{", "old", "new"),
            Err(StructuralError::InvalidJson(_))
        ));
        let json = r#"{"aligned_lines":[[99,99],[0,0]],"chunks":[],"status":"changed"}"#;
        let file = parse_difft_json(json, "old", "new").unwrap();
        assert_eq!(file.rows.len(), 1);
        assert_eq!(file.rows[0].lhs.as_ref().unwrap().text, "old");
    }

    #[test]
    fn status_only_created_and_deleted_results_use_full_input_text() {
        let created = parse_difft_json(
            r#"{"language":"Rust","status":"created"}"#,
            "",
            "fn main() {}\n",
        )
        .unwrap();
        assert_eq!(created.rows.len(), 2);
        assert!(created.rows[0].lhs.is_none());
        assert_eq!(created.rows[0].rhs.as_ref().unwrap().text, "fn main() {}");

        let deleted = parse_difft_json(
            r#"{"language":"Rust","status":"deleted"}"#,
            "fn old() {}\n",
            "",
        )
        .unwrap();
        assert_eq!(deleted.rows.len(), 2);
        assert!(deleted.rows[0].rhs.is_none());
        assert_eq!(deleted.rows[0].lhs.as_ref().unwrap().line_number, 1);
    }

    #[test]
    fn resolver_reports_missing_and_wrong_versions() {
        assert_eq!(
            resolve_candidates(Vec::new()),
            Err(StructuralError::MissingBinary)
        );
        #[cfg(unix)]
        {
            let script = test_script("wrong", "echo 'Difftastic 0.68.0'");
            assert_eq!(
                resolve_candidates(vec![script.clone()]),
                Err(StructuralError::WrongVersion("0.68.0".into()))
            );
            let bundled = test_script("pinned", "echo 'Difftastic 0.69.0'");
            assert_eq!(
                resolve_candidates(vec![script.clone(), bundled.clone()]),
                Ok(bundled.clone()),
                "a wrong PATH version must not hide a valid bundled sidecar"
            );
            let _ = std::fs::remove_file(script);
            let _ = std::fs::remove_file(bundled);
        }
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_failures_and_timeouts_are_typed() {
        let crash = test_script(
            "crash",
            "if [ \"$1\" = --version ]; then echo 'Difftastic 0.69.0'; else echo boom >&2; exit 2; fi",
        );
        assert_eq!(
            run_structural_diff_with_timeout(
                &crash,
                Path::new("crash.rs"),
                "old",
                "new",
                Duration::from_millis(100),
            ),
            Err(StructuralError::Process("boom".into()))
        );

        let timeout = test_script(
            "timeout",
            "if [ \"$1\" = --version ]; then echo 'Difftastic 0.69.0'; else exec sleep 2; fi",
        );
        assert_eq!(
            run_structural_diff_with_timeout(
                &timeout,
                Path::new("timeout.rs"),
                "old",
                "new",
                Duration::from_millis(30),
            ),
            Err(StructuralError::Timeout)
        );
        let _ = std::fs::remove_file(crash);
        let _ = std::fs::remove_file(timeout);
    }

    #[test]
    fn real_binary_replays_fixture_when_explicitly_configured() {
        let Some(binary) = std::env::var_os("TCODE_TEST_DIFFT") else {
            return;
        };
        let result = run_structural_diff_with_binary(
            Path::new(&binary),
            Path::new("fixture.rs"),
            BEFORE,
            AFTER,
        )
        .unwrap();
        assert_eq!(result.status, StructuralStatus::Changed);
        assert_eq!(result.rows.len(), 8);
        assert!(result.rows.iter().filter(|row| row.changed).count() >= 4);
    }

    #[cfg(unix)]
    fn test_script(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path =
            std::env::temp_dir().join(format!("tcode-difftastic-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }
}
