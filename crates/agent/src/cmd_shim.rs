//! The `.cmd` launchers JavaScript package managers write for a package's
//! bin on Windows: npm's `cmd-shim`, pnpm's `@zkochan/cmd-shim` and Yarn
//! Classic's older one.
//!
//! Windows runs a `.cmd` only through `cmd.exe`, whose command line ends at
//! a line break, so Rust refuses to pass such an argument to one. A shim
//! only starts node on a script (or an executable) with its own arguments,
//! so a recognised shim's target is started directly instead. Only the exact
//! templates are recognised: any other launcher can do anything.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What a recognised shim starts, read from the shim's own text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdShim {
    /// The target relative to the shim's directory, `\`-separated as the
    /// shim writes it.
    pub target: String,
    /// The shim runs the target with node: `node.exe` beside the shim, else
    /// `node` from `PATH`. Otherwise it runs the target itself.
    pub node: bool,
    node_path: Option<NodePath>,
}

/// `NODE_PATH` as the shim sets it, `%~dp0` still unexpanded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NodePath {
    Replace(String),
    Prepend(String),
}

/// The process a recognised shim would start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShimLaunch {
    pub program: PathBuf,
    /// Arguments before the caller's own.
    pub args: Vec<OsString>,
    pub node_path: Option<OsString>,
}

impl CmdShim {
    pub fn parse(text: &str) -> Option<Self> {
        let lines: Vec<_> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if let Some(body) = lines.strip_prefix(&NPM_HEADER) {
            return npm(body);
        }
        // Yarn Classic's older cmd-shim sets NODE_PATH first and puts SETLOCAL
        // inside the fallback branch.
        let (node_path, legacy) = match lines
            .first()
            .and_then(|line| line.strip_prefix("@SET NODE_PATH="))
        {
            Some(paths) => (Some(NodePath::Replace(plain(paths)?.into())), &lines[1..]),
            None => (None, &lines[..]),
        };
        if let Some(mut shim) = legacy_body(legacy) {
            shim.node_path = node_path;
            return Some(shim);
        }
        let body = lines.strip_prefix(&["@SETLOCAL"])?;
        let (node_path, body) = if body.first() == Some(&"@IF NOT DEFINED NODE_PATH (") {
            if body.len() < 5 {
                return None;
            }
            let paths = plain(
                body[1]
                    .strip_prefix("@SET \"NODE_PATH=")
                    .and_then(|line| line.strip_suffix('"'))?,
            )?;
            if body[2] != ") ELSE ("
                || body[3] != format!("@SET \"NODE_PATH={paths};%NODE_PATH%\"")
                || body[4] != ")"
            {
                return None;
            }
            (Some(NodePath::Prepend(paths.into())), &body[5..])
        } else {
            (None, body)
        };
        let mut shim = pnpm_body(body)?;
        shim.node_path = node_path;
        Some(shim)
    }

    /// The process this shim, at `shim`, would start, looking for node in
    /// `path_dirs` when none is beside it. `None` when that is not a program
    /// Windows starts by itself.
    fn launch_in(
        &self,
        shim: &Path,
        path_dirs: impl IntoIterator<Item = PathBuf>,
    ) -> Option<ShimLaunch> {
        let dir = shim.parent()?;
        let target = self
            .target
            .split('\\')
            .fold(dir.to_path_buf(), |path, part| path.join(part));
        let node_path = self.node_path.as_ref().map(|node_path| {
            let expand =
                |paths: &str| paths.replace("%~dp0", &format!("{}\\", dir.to_string_lossy()));
            match node_path {
                NodePath::Replace(paths) => OsString::from(expand(paths)),
                NodePath::Prepend(paths) => {
                    let mut value = OsString::from(expand(paths));
                    // `IF NOT DEFINED`: cmd treats an empty variable as undefined.
                    if let Some(existing) =
                        std::env::var_os("NODE_PATH").filter(|value| !value.is_empty())
                    {
                        value.push(";");
                        value.push(existing);
                    }
                    value
                }
            }
        });
        if !self.node {
            let runnable = target
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("com")
                });
            return runnable.then_some(ShimLaunch {
                program: target,
                args: Vec::new(),
                node_path,
            });
        }
        let beside = dir.join("node.exe");
        let program = if beside.is_file() {
            beside
        } else {
            // cmd.exe would look in the child's working directory first; that
            // is the project, whose files must not decide what runs.
            let extensions: Vec<_> = crate::path_extensions()
                .into_iter()
                .filter(|extension| !extension.eq_ignore_ascii_case(".js"))
                .collect();
            crate::find_in_dirs(path_dirs, "node", &extensions, |path| {
                path.extension().is_some() && crate::is_executable(path)
            })?
        };
        Some(ShimLaunch {
            program,
            args: vec![target.into_os_string()],
            node_path,
        })
    }
}

/// What `program` runs when it is a recognised shim.
pub(crate) fn launch(program: &Path) -> Option<ShimLaunch> {
    if !program
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"))
    {
        return None;
    }
    let text = std::fs::read_to_string(program).ok()?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    CmdShim::parse(&text)?.launch_in(program, std::env::split_paths(&path))
}

const NPM_HEADER: [&str; 8] = [
    "@ECHO off",
    "GOTO start",
    ":find_dp0",
    "SET dp0=%~dp0",
    "EXIT /b",
    ":start",
    "SETLOCAL",
    "CALL :find_dp0",
];

fn npm(body: &[&str]) -> Option<CmdShim> {
    if let [launch] = body {
        return direct(launch, "%dp0%");
    }
    let body = body.strip_prefix(&[
        "IF EXIST \"%dp0%\\node.exe\" (",
        "SET \"_prog=%dp0%\\node.exe\"",
        ") ELSE (",
        "SET \"_prog=node\"",
    ])?;
    let body = body
        .strip_prefix(&["SET PATHEXT=%PATHEXT:;.JS;=;%"])
        .unwrap_or(body);
    let [")", launch] = body else {
        return None;
    };
    let launch = [
        "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"",
        "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\"",
    ]
    .into_iter()
    .find_map(|prefix| launch.strip_prefix(prefix))?;
    Some(CmdShim {
        target: invoked(launch.trim(), "%dp0%")?.into(),
        node: true,
        node_path: None,
    })
}

fn legacy_body(body: &[&str]) -> Option<CmdShim> {
    if let [launch] = body {
        return direct(launch.trim_start_matches('@'), "%~dp0");
    }
    if body.len() != 7
        || body[0] != "@IF EXIST \"%~dp0\\node.exe\" ("
        || body[2] != ") ELSE ("
        || body[3] != "@SETLOCAL"
        || body[4] != "@SET PATHEXT=%PATHEXT:;.JS;=;%"
        || body[6] != ")"
    {
        return None;
    }
    node_branches(body[1], body[5])
}

fn pnpm_body(body: &[&str]) -> Option<CmdShim> {
    if let [launch] = body {
        return direct(launch.trim_start_matches('@'), "%~dp0");
    }
    if body.len() != 6
        || body[0] != "@IF EXIST \"%~dp0\\node.exe\" ("
        || body[2] != ") ELSE ("
        || body[3] != "@SET PATHEXT=%PATHEXT:;.JS;=;%"
        || body[5] != ")"
    {
        return None;
    }
    node_branches(body[1], body[4])
}

/// The two branches of `IF EXIST node.exe`, which must run the same target.
fn node_branches(beside: &str, on_path: &str) -> Option<CmdShim> {
    let beside = invoked(beside.strip_prefix("\"%~dp0\\node.exe\"")?.trim(), "%~dp0")?;
    let on_path = invoked(on_path.strip_prefix("node ")?.trim(), "%~dp0")?;
    (beside == on_path).then(|| CmdShim {
        target: beside.into(),
        node: true,
        node_path: None,
    })
}

fn direct(launch: &str, dir: &str) -> Option<CmdShim> {
    Some(CmdShim {
        target: invoked(launch, dir)?.into(),
        node: false,
        node_path: None,
    })
}

/// The target of `"<dir>\<target>" %*`.
fn invoked<'a>(line: &'a str, dir: &str) -> Option<&'a str> {
    let rest = line
        .strip_prefix('"')?
        .strip_prefix(dir)?
        .strip_prefix('\\')?;
    let (target, rest) = rest.split_once('"')?;
    (!target.is_empty() && !target.contains('%') && rest.trim() == "%*").then_some(target)
}

/// NODE_PATH entries with nothing cmd would expand or interpret besides `%~dp0`.
fn plain(paths: &str) -> Option<&str> {
    (!paths
        .replace("%~dp0", "")
        .contains(['%', '"', '&', '|', '<', '>', '^']))
    .then_some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Written by npm's cmd-shim 8.0.0 (lib/index.js `writeShim_`) for
    // `@anthropic-ai/claude-code`, whose bin starts `#!/usr/bin/env node`.
    const NPM_NODE: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\cli.js\" %*\r\n";
    // The same template for a bin without a shebang.
    const NPM_DIRECT: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.exe\"   %*\r\n";
    // pnpm's @zkochan/cmd-shim for a global package.
    const PNPM_NODE: &str = "@SETLOCAL\r\n@IF NOT DEFINED NODE_PATH (\r\n  @SET \"NODE_PATH=%~dp0\\global\\5\\node_modules\"\r\n) ELSE (\r\n  @SET \"NODE_PATH=%~dp0\\global\\5\\node_modules;%NODE_PATH%\"\r\n)\r\n@IF EXIST \"%~dp0\\node.exe\" (\r\n  \"%~dp0\\node.exe\"  \"%~dp0\\global\\5\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n) ELSE (\r\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n  node  \"%~dp0\\global\\5\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n)\r\n";
    // Yarn Classic's older cmd-shim.
    const YARN_NODE: &str = "@IF EXIST \"%~dp0\\node.exe\" (\r\n  \"%~dp0\\node.exe\"  \"%~dp0\\..\\global\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n) ELSE (\r\n  @SETLOCAL\r\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n  node  \"%~dp0\\..\\global\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n)";
    // What npm installs for itself beside node.exe: not a cmd-shim.
    const NPM_OWN: &str = ":: Created by npm, please don't edit manually.\r\n@ECHO OFF\r\n\r\nSETLOCAL\r\n\r\nSET \"NODE_EXE=%~dp0\\node.exe\"\r\nIF NOT EXIST \"%NODE_EXE%\" (\r\n  SET \"NODE_EXE=node\"\r\n)\r\n\r\nSET \"NPM_PREFIX_JS=%~dp0\\node_modules\\npm\\bin\\npm-prefix.js\"\r\nSET \"NPM_CLI_JS=%~dp0\\node_modules\\npm\\bin\\npm-cli.js\"\r\nFOR /F \"delims=\" %%F IN ('CALL \"%NODE_EXE%\" \"%NPM_PREFIX_JS%\"') DO (\r\n  SET \"NPM_PREFIX_NPM_CLI_JS=%%F\\node_modules\\npm\\bin\\npm-cli.js\"\r\n)\r\nIF EXIST \"%NPM_PREFIX_NPM_CLI_JS%\" (\r\n  SET \"NPM_CLI_JS=%NPM_PREFIX_NPM_CLI_JS%\"\r\n)\r\n\r\n\"%NODE_EXE%\" \"%NPM_CLI_JS%\" %*\r\n";

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tcode-cmd-shim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn executable(path: &Path) {
        std::fs::write(path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn package_manager_shims_name_what_they_run_and_nothing_else_is_read() {
        let node = |target: &str| CmdShim {
            target: target.into(),
            node: true,
            node_path: None,
        };
        assert_eq!(
            CmdShim::parse(NPM_NODE),
            Some(node("node_modules\\@anthropic-ai\\claude-code\\cli.js"))
        );
        assert_eq!(
            CmdShim::parse(NPM_DIRECT),
            Some(CmdShim {
                node: false,
                ..node("node_modules\\@openai\\codex\\bin\\codex.exe")
            })
        );
        assert_eq!(
            CmdShim::parse(PNPM_NODE),
            Some(CmdShim {
                node_path: Some(NodePath::Prepend("%~dp0\\global\\5\\node_modules".into())),
                ..node("global\\5\\node_modules\\@openai\\codex\\bin\\codex.js")
            })
        );
        assert_eq!(
            CmdShim::parse(YARN_NODE),
            Some(node(
                "..\\global\\node_modules\\@openai\\codex\\bin\\codex.js"
            ))
        );
        for text in [
            NPM_OWN.to_string(),
            format!("{NPM_NODE}echo custom-command\r\n"),
            NPM_NODE.replace("\"%_prog%\"  ", "\"%_prog%\" --no-warnings "),
            NPM_NODE.replace("cli.js", "%CLI%"),
            YARN_NODE.replacen("codex.js", "other.js", 1),
        ] {
            assert_eq!(CmdShim::parse(&text), None, "{text}");
        }
    }

    #[test]
    fn a_node_shim_runs_node_beside_it_else_node_from_path() {
        let dir = scratch("node");
        let bin = dir.join("npm");
        let elsewhere = dir.join("nodejs");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let shim = bin.join("claude.cmd");
        let parsed = CmdShim::parse(NPM_NODE).unwrap();
        let script = bin
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code")
            .join("cli.js");

        assert_eq!(parsed.launch_in(&shim, [elsewhere.clone()]), None);

        let on_path = elsewhere.join(if cfg!(windows) { "node.exe" } else { "node" });
        executable(&on_path);
        // Without an extension Windows would not start it; elsewhere `node`
        // has none, so only Windows can tell the two apart.
        executable(&bin.join("node"));
        let found = parsed.launch_in(&shim, [bin.clone(), elsewhere.clone()]);
        if cfg!(windows) {
            // PATHEXT names the extension, upper-case by default; the file
            // system does not care.
            let found = found.unwrap();
            assert!(
                found
                    .program
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&on_path.to_string_lossy())
            );
            assert_eq!(found.args, vec![OsString::from(script.clone())]);
        }

        executable(&bin.join("node.exe"));
        assert_eq!(
            parsed.launch_in(&shim, [elsewhere]),
            Some(ShimLaunch {
                program: bin.join("node.exe"),
                args: vec![script.into()],
                node_path: None,
            })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_shim_without_node_runs_an_executable_target_and_sets_its_node_path() {
        let shim = Path::new("/pnpm/codex.cmd");
        assert_eq!(
            CmdShim::parse(NPM_DIRECT).unwrap().launch_in(shim, []),
            Some(ShimLaunch {
                program: Path::new("/pnpm")
                    .join("node_modules")
                    .join("@openai")
                    .join("codex")
                    .join("bin")
                    .join("codex.exe"),
                args: Vec::new(),
                node_path: None,
            })
        );
        let script = CmdShim {
            target: "bin\\codex.js".into(),
            node: false,
            node_path: None,
        };
        assert_eq!(script.launch_in(shim, []), None);

        let dir = scratch("node-path");
        executable(&dir.join("node.exe"));
        let launch = CmdShim::parse(PNPM_NODE)
            .unwrap()
            .launch_in(&dir.join("codex.cmd"), [])
            .unwrap();
        let expected = format!("{}\\\\global\\5\\node_modules", dir.to_string_lossy());
        match std::env::var_os("NODE_PATH").filter(|value| !value.is_empty()) {
            None => assert_eq!(launch.node_path, Some(expected.into())),
            Some(existing) => {
                let mut value = OsString::from(format!("{expected};"));
                value.push(existing);
                assert_eq!(launch.node_path, Some(value));
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The shim's program, started directly, receives every argument as
    /// written: through `cmd.exe` one that spans lines cannot be passed.
    #[cfg(windows)]
    #[test]
    fn an_npm_shim_passes_an_argument_that_spans_lines() {
        const NAME: &str = "cmd_shim::tests::an_npm_shim_passes_an_argument_that_spans_lines";
        if let Some(out) = std::env::var_os("TCODE_TEST_SHIM_ARGV") {
            let args: Vec<_> = std::env::args().skip(1).collect();
            std::fs::write(out, args.join("\0")).unwrap();
            return;
        }
        let dir = scratch("spawn");
        let shim = dir.join("claude.cmd");
        std::fs::write(&shim, NPM_NODE).unwrap();
        // The test binary stands in for node: run with `--exact` and its own
        // name, it records what it was given.
        std::fs::copy(std::env::current_exe().unwrap(), dir.join("node.exe")).unwrap();
        let script = dir
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code")
            .join("cli.js");
        let out = dir.join("argv");
        let spanning = "first line\r\nsecond line\n";
        let status = crate::process::command(&shim)
            .args(["--exact", NAME, "--nocapture", "--quiet", spanning])
            .env("TCODE_TEST_SHIM_ARGV", &out)
            .status()
            .unwrap();
        assert!(status.success());
        let script = script.to_string_lossy();
        assert_eq!(
            std::fs::read_to_string(&out)
                .unwrap()
                .split('\0')
                .collect::<Vec<_>>(),
            [
                &*script,
                "--exact",
                NAME,
                "--nocapture",
                "--quiet",
                spanning
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A batch file that is not a shim still runs through `cmd.exe`, so a
    /// provider start that would give it an argument spanning lines names the
    /// launcher instead of failing in std's words.
    #[cfg(windows)]
    #[test]
    fn a_batch_launcher_that_is_not_a_shim_is_refused_an_argument_that_spans_lines() {
        let dir = scratch("batch");
        let launcher = dir.join("pi.cmd");
        std::fs::write(&launcher, "@echo off\r\nnode \"%~dp0\\pi.js\" %*\r\n").unwrap();
        let start = |extra_args: Vec<String>| {
            smol::block_on(crate::start_session(
                crate::ProviderKind::Pi,
                crate::SessionOptions {
                    cwd: dir.clone(),
                    model: None,
                    resume: None,
                    fork: None,
                    binary_path: Some(launcher.clone()),
                    option_selections: Vec::new(),
                    mcp_servers: Vec::new(),
                    launch_env: crate::LaunchEnv::default(),
                    extra_args,
                    acp: None,
                    instructions: None,
                    processes: Default::default(),
                    clock: Default::default(),
                },
            ))
        };
        match start(vec!["--append-system-prompt".into(), "one\ntwo".into()]) {
            Err(crate::AgentError::BatchLineBreak { program }) => assert_eq!(program, launcher),
            other => panic!("{:?}", other.err()),
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
