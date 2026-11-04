//! EmmyLua annotations kept in C sources.
//!
//! A C source that implements a Lua API can keep the annotations of that
//! API next to the implementation, in `/** ... */` comments written as in
//! Lua with ` * ` in place of `---`:
//!
//! ```c
//! /**
//!  * Greet someone.
//!  *
//!  * @param name string who to greet
//!  * @return string greeting
//!  * function mylib.greet(name) end
//!  */
//! static int mylib_greet(lua_State *L)
//! ```
//!
//! A line that holds a Lua statement (`function <name>(<params>) end`,
//! `local <name> = {}` or `return <name>`) says what the annotations above
//! it describe. A comment with such a line, or one that defines a type
//! (`@class`, `@enum`, `@alias`), is a Lua doc comment; other comments, such
//! as the documentation of C functions, are left out. A comment opens with
//! a line that is exactly `/**` and closes at the first line containing
//! `*/`.
//!
//! The source is analyzed as the Lua code these comments make: every other
//! line becomes empty, so lines keep their numbers, and a Lua doc comment
//! keeps its columns, so every position reported for it points into the C
//! source. The first line, which no comment's text is on, turns off
//! `missing-return` and `unused`: the declarations have empty bodies.

use std::path::Path;

const C_EXTENSIONS: &[&str] = &["c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx"];

const TYPE_TAGS: &[&str] = &["@class", "@enum", "@alias"];

const DIRECTIVE: &str = "---@diagnostic disable: missing-return, unused";

pub(crate) fn is_c_source(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| C_EXTENSIONS.contains(&ext))
}

/// `mylib.c` -> `mylib`.
pub(crate) fn strip_c_extension(path: &str) -> Option<&str> {
    let (stem, ext) = path.rsplit_once('.')?;
    C_EXTENSIONS.contains(&ext).then_some(stem)
}

/// The Lua code that the Lua doc comments of a C source make.
pub(crate) fn c_source_to_lua(source: &str) -> String {
    if source.is_empty() {
        return String::new();
    }
    let mut out = String::from(DIRECTIVE);
    let mut in_comment = false;
    let mut is_lua_doc_comment = false;
    // Whether a comment is a Lua doc comment is known only at its end, so its lines wait here.
    let mut current_comment = Vec::new();
    for line in source.split_inclusive('\n') {
        let content = strip_line_ending(line);
        if !in_comment {
            in_comment = content == "/**";
            out.push_str(line_ending(line));
        } else if content.contains("*/") {
            for comment_line in current_comment.drain(..) {
                if is_lua_doc_comment {
                    out.push_str(&line_to_lua(strip_line_ending(comment_line)));
                }
                out.push_str(line_ending(comment_line));
            }
            out.push_str(line_ending(line));
            in_comment = false;
            is_lua_doc_comment = false;
        } else {
            let text = strip_comment_prefix(content);
            is_lua_doc_comment |= is_statement(text) || defines_type(text);
            current_comment.push(line);
        }
    }
    // A comment left open at the end of the file is left out.
    for comment_line in current_comment {
        out.push_str(line_ending(comment_line));
    }
    out
}

/// ` * text` -> `---text`; a statement gets spaces in place of ` * `, to keep its columns.
fn line_to_lua(content: &str) -> String {
    let text = strip_comment_prefix(content);
    if is_statement(text) {
        format!("{}{}", " ".repeat(content.len() - text.len()), text)
    } else {
        format!("---{}", text)
    }
}

fn strip_comment_prefix(content: &str) -> &str {
    content
        .strip_prefix(" * ")
        .or_else(|| content.strip_prefix(" *"))
        .unwrap_or(content)
}

fn defines_type(text: &str) -> bool {
    TYPE_TAGS
        .iter()
        .any(|tag| text == *tag || text.starts_with(&format!("{tag} ")))
}

fn is_statement(text: &str) -> bool {
    // An indented line is a code example.
    if text.starts_with(char::is_whitespace) {
        return false;
    }
    let spaced = text
        .replace('(', " ( ")
        .replace(')', " ) ")
        .replace('=', " = ");
    let words: Vec<&str> = spaced.split_whitespace().collect();
    match words.as_slice() {
        ["function", name, "(", .., ")", "end"] => is_name_path(name),
        ["local", name, "=", "{}"] => is_name(name),
        ["return", name] => is_name(name),
        _ => false,
    }
}

/// `a`, `a.b.c`, `a.b:c`.
fn is_name_path(text: &str) -> bool {
    let (path, method) = match text.split_once(':') {
        Some((path, method)) => (path, Some(method)),
        None => (text, None),
    };
    path.split('.').all(is_name) && method.is_none_or(is_name)
}

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn strip_line_ending(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .unwrap_or(line)
}

fn line_ending(line: &str) -> &str {
    &line[strip_line_ending(line).len()..]
}

#[cfg(test)]
mod tests {
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{DiagnosticCode, FileId, VirtualWorkspace};

    const SOURCE: &str = "\
#include <lua.h>

/**
 * Greet someone.
 *
 * @param name string who to greet
 * @return string greeting
 * function mylib.greet(name) end
 */
static int mylib_greet(lua_State *L)
{
\t/* A comment inside a function. */
\treturn 1;
}

/**
 * Check the arguments of a call.
 *
 * @param L the Lua state
 */
static void check_args(lua_State *L);
";

    #[test]
    fn test_keeps_lines_and_columns() {
        let lua = c_source_to_lua(SOURCE);
        assert_eq!(lua.lines().count(), SOURCE.lines().count());
        let lines: Vec<&str> = lua.lines().collect();
        assert_eq!(lines[3], "---Greet someone.");
        assert_eq!(lines[4], "---");
        assert_eq!(lines[5], "---@param name string who to greet");
        assert_eq!(lines[7], "   function mylib.greet(name) end");
        let c_lines: Vec<&str> = SOURCE.lines().collect();
        assert_eq!(lines[5].find("string"), c_lines[5].find("string"));
    }

    #[test]
    fn test_leaves_out_c_code_and_c_comments() {
        let lua = c_source_to_lua(SOURCE);
        let kept: Vec<&str> = lua
            .lines()
            .skip(1)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(kept.len(), 5);
        assert!(!lua.contains("the Lua state"));
        assert!(!lua.contains("mylib_greet"));
    }

    #[test]
    fn test_comment_defining_a_type() {
        let lua = c_source_to_lua("/**\n * A counter.\n *\n * @class mylib.counter\n */\n");
        assert_eq!(
            lua,
            format!("{DIRECTIVE}\n---A counter.\n---\n---@class mylib.counter\n\n")
        );
    }

    #[test]
    fn test_statements() {
        assert!(is_statement("function mylib.greet(name) end"));
        assert!(is_statement("function counter:add(n) end"));
        assert!(is_statement("local mylib = {}"));
        assert!(is_statement("local mylib={}"));
        assert!(is_statement("return mylib"));
        assert!(!is_statement("function arguments are checked first."));
        assert!(!is_statement("local variables of the caller stay intact."));
        assert!(!is_statement("return 0 on success."));
        assert!(!is_statement("    return mylib"));
    }

    #[test]
    fn test_crlf() {
        let lua = c_source_to_lua("/**\r\n * @alias mylib.mode string\r\n */\r\nint x;\r\n");
        assert_eq!(
            lua,
            format!("{DIRECTIVE}\r\n---@alias mylib.mode string\r\n\r\n\r\n")
        );
    }

    #[test]
    fn test_unterminated_comment() {
        let lua = c_source_to_lua("int x;\n/**\n * @class mylib\n * local mylib = {}\n");
        assert_eq!(lua, format!("{DIRECTIVE}\n\n\n\n"));
    }

    #[test]
    fn test_empty_source() {
        assert_eq!(c_source_to_lua(""), "");
    }

    #[test]
    fn test_strip_c_extension() {
        assert_eq!(strip_c_extension("src/mylib.c"), Some("src/mylib"));
        assert_eq!(strip_c_extension("src/mylib.lua"), None);
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

    fn diagnostic_codes(ws: &VirtualWorkspace, file_id: FileId) -> Vec<String> {
        let mut codes: Vec<String> = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|diagnostic| match diagnostic.code {
                Some(NumberOrString::String(code)) => Some(code),
                _ => None,
            })
            .collect();
        codes.sort();
        codes
    }

    #[test]
    fn test_require_c_source() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file("src/mylib.c", MODULE);
        let file_id = ws.def(
            r#"
            local mylib = require("mylib")
            ---@type integer
            local n = mylib.greet("world")
            print(n, mylib.greet(42))
            "#,
        );
        assert_eq!(
            diagnostic_codes(&ws, file_id),
            vec!["assign-type-mismatch", "param-type-mismatch"]
        );
    }

    #[test]
    fn test_declarations_are_not_diagnosed_for_their_bodies() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file("src/mylib.c", MODULE);
        assert_eq!(diagnostic_codes(&ws, file_id), Vec::<String>::new());
    }

    #[test]
    fn test_c_source_positions() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let source = MODULE.replace("name string", "name mylib.missing");
        let file_id = ws.def_file("src/mylib.c", &source);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let found: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::TypeNotFound.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.range.start)
            .collect();
        let line = source
            .lines()
            .position(|line| line.contains("mylib.missing"));
        let column = source.lines().find_map(|line| line.find("mylib.missing"));
        assert_eq!(found.len(), 1, "{diagnostics:#?}");
        assert_eq!(Some(found[0].line as usize), line);
        assert_eq!(Some(found[0].character as usize), column);
    }
}
