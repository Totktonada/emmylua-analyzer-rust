//! Input filters: programs a project names in `workspace.inputFilters` to
//! turn files of another language into the Lua text the analyzer reads, eg:
//! the EmmyLua annotations kept in the comments of a C source.
//!
//! A filter gets the file's text on its standard input and the file's path
//! in place of `${file}` in its arguments, and prints the Lua text. A filter
//! that keeps the file's lines keeps every reported position in the file
//! itself. A file whose filter fails or times out is read as empty, never as
//! itself.
//!
//! Whether to run the programs a configuration names is decided before the
//! configuration gets here: the language server asks the user, the command
//! line tools take a flag.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use wax::{Glob, Pattern};

use crate::EmmyrcInputFilter;

#[derive(Debug, Default)]
pub(crate) struct InputFilters {
    filters: Vec<(Glob<'static>, EmmyrcInputFilter)>,
}

impl InputFilters {
    pub(crate) fn new(configs: &[EmmyrcInputFilter]) -> Self {
        let filters = configs
            .iter()
            .filter_map(|config| match Glob::new(&path_glob(&config.glob)) {
                Ok(glob) => Some((glob.into_owned(), config.clone())),
                Err(err) => {
                    log::error!("Invalid input filter glob {:?}: {}", config.glob, err);
                    None
                }
            })
            .collect();
        Self { filters }
    }

    /// The text to analyze for the file at `path`: its own text, or what its
    /// filter makes of it.
    pub(crate) fn apply(&self, path: &Path, text: String) -> String {
        let Some((_, filter)) = self.filters.iter().find(|(glob, _)| glob.is_match(path)) else {
            return text;
        };
        match run(filter, path, &text) {
            Ok(lua) => {
                if lua.lines().count() != text.lines().count() {
                    log::warn!(
                        "Input filter {:?} changed the number of lines of {:?}: positions in it will be off",
                        filter.program,
                        path
                    );
                }
                lua
            }
            Err(err) => {
                log::error!("Input filter {:?} on {:?}: {}", filter.program, path, err);
                String::new()
            }
        }
    }
}

/// A glob of the workspace, such as `*.c`, as matched against the
/// absolute path of a file: under any directory.
fn path_glob(glob: &str) -> String {
    if glob.starts_with("**/") || Path::new(glob).is_absolute() {
        glob.to_string()
    } else {
        format!("**/{}", glob)
    }
}

fn run(filter: &EmmyrcInputFilter, path: &Path, text: &str) -> Result<String, String> {
    let timeout = Duration::from_millis(filter.timeout);
    let deadline = Instant::now() + timeout;
    let file = path.to_string_lossy();
    let mut child = Command::new(&filter.program)
        .args(filter.args.iter().map(|arg| arg.replace("${file}", &file)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot start it: {}", err))?;

    // The input is written and the output read on threads of their own: a
    // pipe holds only so much, and a filter may print before it has read all
    // of its input, or never read it.
    if let Some(mut stdin) = child.stdin.take() {
        let text = text.to_string();
        thread::spawn(move || {
            // Fails when the filter exits without reading its input.
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let stdout = read_to_end_on_thread(child.stdout.take());
    let stderr = read_to_end_on_thread(child.stderr.take());

    let output = match stdout.recv_timeout(timeout) {
        Ok(output) => output,
        Err(RecvTimeoutError::Timeout) => return Err(kill(&mut child, filter.timeout)),
        Err(RecvTimeoutError::Disconnected) => return Err("no output".to_string()),
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(1)),
            Ok(None) => return Err(kill(&mut child, filter.timeout)),
            Err(err) => return Err(err.to_string()),
        }
    };
    if !status.success() {
        let stderr = stderr
            .recv_timeout(Duration::from_millis(100))
            .unwrap_or_default();
        return Err(format!(
            "{}: {}",
            status,
            String::from_utf8_lossy(&stderr).trim_end()
        ));
    }
    String::from_utf8(output).map_err(|_| "its output is not UTF-8".to_string())
}

fn read_to_end_on_thread(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            let _ = sender.send(data);
        });
    }
    receiver
}

fn kill(child: &mut std::process::Child, timeout: u64) -> String {
    let _ = child.kill();
    let _ = child.wait();
    format!("timed out after {} ms", timeout)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::Emmyrc;

    pub(super) fn filter(glob: &str, program: &str, args: &[&str]) -> EmmyrcInputFilter {
        EmmyrcInputFilter {
            glob: glob.to_string(),
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            timeout: 5000,
        }
    }

    #[test]
    fn test_glob_matches_absolute_paths() {
        let filters = InputFilters::new(&[
            filter("src/*.c", "unused", &[]),
            filter("**/include/*.h", "unused", &[]),
        ]);
        let matches = |path: &str| {
            filters
                .filters
                .iter()
                .any(|(glob, _)| glob.is_match(Path::new(path)))
        };
        assert!(matches("/home/user/project/src/mylib.c"));
        assert!(matches("/home/user/project/include/mylib.h"));
        assert!(!matches("/home/user/project/src/mylib.lua"));
        assert!(!matches("/home/user/project/lib/mylib.c"));
    }

    #[test]
    fn test_other_files_pass_through() {
        let filters = InputFilters::new(&[filter("**/*.c", "program-that-does-not-exist", &[])]);
        let text = "local x = 1\n".to_string();
        assert_eq!(
            filters.apply(Path::new("/project/main.lua"), text.clone()),
            text
        );
    }

    #[test]
    fn test_failing_filter_leaves_file_empty() {
        let filters = InputFilters::new(&[filter("**/*.c", "program-that-does-not-exist", &[])]);
        let text = "int main(void) { return 0; }\n".to_string();
        assert_eq!(filters.apply(Path::new("/project/main.c"), text), "");
    }

    #[test]
    fn test_placeholders() {
        let mut emmyrc: Emmyrc = serde_json::from_str(
            r#"{
                "workspace": {
                    "inputFilters": [{
                        "glob": "**/*.c",
                        "program": "lua",
                        "args": ["${workspaceFolder}/tools/c2lua.lua", "${file}"]
                    }]
                }
            }"#,
        )
        .unwrap();
        let root = PathBuf::from("/home/user/project");
        emmyrc.pre_process_emmyrc(&root);
        let filter = &emmyrc.workspace.input_filters[0];
        assert_eq!(filter.program, "lua");
        assert_eq!(
            filter.args,
            vec![
                format!("{}/tools/c2lua.lua", root.display()),
                "${file}".to_string()
            ]
        );
        assert_eq!(filter.timeout, 5000);
    }

    /// Tests that run programs found on Unix.
    #[cfg(unix)]
    mod unix {
        use lsp_types::{Diagnostic, NumberOrString};
        use tokio_util::sync::CancellationToken;

        use super::super::*;
        use super::filter;
        use crate::{DiagnosticCode, EmmyrcWorkspaceModuleMap, FileId, VirtualWorkspace};

        /// A project's own filter: the EmmyLua annotations of a C source's
        /// comments, with ` * @` written for `---@` and a Lua statement after
        /// ` * `, become Lua on the same lines and columns; other lines empty.
        fn c_filter() -> EmmyrcInputFilter {
            filter(
                "**/*.c",
                "sed",
                &[
                    "-E",
                    "-e",
                    r"s/^ \* (@.*)$/---\1/",
                    "-e",
                    r"s/^ \* ((function|local|return) .*)$/   \1/",
                    "-e",
                    r"/^(---|   )/!s/.*//",
                ],
            )
        }

        const MODULE: &str = "\
/**
 * @class mylib
 * local mylib = {}
 */

/**
 * @param name string who to greet
 * @return string
 * function mylib.greet(name) end
 */
static int mylib_greet(lua_State *L);

/**
 * return mylib
 */
";

        fn workspace_with_c_filter() -> VirtualWorkspace {
            let mut ws = VirtualWorkspace::new_with_init_std_lib();
            let mut emmyrc = ws.get_emmyrc();
            emmyrc.workspace.input_filters = vec![c_filter()];
            // `require("mylib")` finds `mylib.c`.
            emmyrc.workspace.module_map = vec![EmmyrcWorkspaceModuleMap {
                pattern: r"^(.*)\.c$".to_string(),
                replace: "$1".to_string(),
            }];
            ws.update_emmyrc(emmyrc);
            ws
        }

        fn diagnostics(ws: &VirtualWorkspace, file_id: FileId) -> Vec<Diagnostic> {
            ws.analysis
                .diagnose_file(file_id, CancellationToken::new())
                .unwrap_or_default()
        }

        fn code(diagnostic: &Diagnostic) -> Option<&str> {
            match &diagnostic.code {
                Some(NumberOrString::String(code)) => Some(code),
                _ => None,
            }
        }

        #[test]
        fn test_filter_output_is_analyzed() {
            let mut ws = workspace_with_c_filter();
            ws.def_file("src/mylib.c", MODULE);
            let file_id = ws.def(
                r#"
                local mylib = require("mylib")
                ---@type integer
                local n = mylib.greet("world")
                print(n, mylib.greet(42))
                "#,
            );
            let mut codes: Vec<&str> = Vec::new();
            let all = diagnostics(&ws, file_id);
            codes.extend(all.iter().filter_map(code));
            codes.sort();
            assert_eq!(codes, vec!["assign-type-mismatch", "param-type-mismatch"]);
        }

        #[test]
        fn test_positions_point_into_the_file() {
            let mut ws = workspace_with_c_filter();
            let source = MODULE.replace("name string", "name mylib.missing");
            let file_id = ws.def_file("src/mylib.c", &source);
            let all = diagnostics(&ws, file_id);
            let found: Vec<_> = all
                .iter()
                .filter(|diagnostic| {
                    code(diagnostic) == Some(DiagnosticCode::TypeNotFound.get_name())
                })
                .map(|diagnostic| diagnostic.range.start)
                .collect();
            let line = source
                .lines()
                .position(|line| line.contains("mylib.missing"));
            let column = source.lines().find_map(|line| line.find("mylib.missing"));
            assert_eq!(found.len(), 1, "{all:#?}");
            assert_eq!(Some(found[0].line as usize), line);
            assert_eq!(Some(found[0].character as usize), column);
        }

        #[test]
        fn test_timeout_leaves_file_empty() {
            let mut slow = filter("**/*.c", "sleep", &["10"]);
            slow.timeout = 100;
            let filters = InputFilters::new(&[slow]);
            let start = Instant::now();
            let text = filters.apply(Path::new("/project/main.c"), MODULE.to_string());
            assert_eq!(text, "");
            assert!(start.elapsed() < Duration::from_secs(5));
        }

        #[test]
        fn test_filter_that_does_not_read_its_input() {
            // More than a pipe holds, to a filter that never reads it.
            let text = "/* filler */\n".repeat(100_000);
            let filters = InputFilters::new(&[filter("**/*.c", "echo", &["local x = 1"])]);
            assert_eq!(
                filters.apply(Path::new("/project/main.c"), text),
                "local x = 1\n"
            );
        }
    }
}
