//! `.env` file loading (§9.2). Values are read when a process starts, passed to
//! that process only, and kept in memory for masking. They are never stored.

use std::path::Path;

/// Parses dotenv syntax: `KEY=value`, optional `export `, `#` comments,
/// single quotes (literal) and double quotes (with `\n`, `\"`, `\\` escapes).
pub fn parse(src: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut lines = src.lines().enumerate();
    while let Some((n, raw)) = lines.next() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, rest)) = line.split_once('=') else {
            return Err(format!("line {}: expected KEY=value", n + 1));
        };
        let key = key.trim();
        if !crate::config::is_env_name(key) {
            return Err(format!("line {}: `{key}` is not a valid variable name", n + 1));
        }
        let rest = rest.trim_start();
        let value = if let Some(body) = rest.strip_prefix('"') {
            let mut text = body.to_string();
            // Multi-line double-quoted values.
            while !ends_unescaped_quote(&text) {
                match lines.next() {
                    Some((_, more)) => {
                        text.push('\n');
                        text.push_str(more);
                    }
                    None => return Err(format!("line {}: unterminated double quote", n + 1)),
                }
            }
            let end = closing_quote(&text).unwrap_or(text.len());
            unescape(&text[..end])
        } else if let Some(body) = rest.strip_prefix('\'') {
            match body.find('\'') {
                Some(end) => body[..end].to_string(),
                None => return Err(format!("line {}: unterminated single quote", n + 1)),
            }
        } else {
            let v = match rest.find(" #") {
                Some(i) => &rest[..i],
                None => rest,
            };
            v.trim_end().to_string()
        };
        out.push((key.to_string(), value));
    }
    Ok(out)
}

fn closing_quote(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some(i),
            _ => escaped = false,
        }
    }
    None
}

fn ends_unescaped_quote(s: &str) -> bool {
    closing_quote(s).is_some()
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn load(path: &Path) -> Result<Vec<(String, String)>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("cannot read env_file {}: {e}", path.display()))?;
    parse(&src).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        let src = "# comment\nexport A=1\nB = two words # trailing\nC='lit $x # no'\nD=\"q \\\"x\\\" \\n end\"\nE=\n\nF=\"multi\nline\"\n";
        let v = parse(src).unwrap();
        assert_eq!(
            v,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "lit $x # no".into()),
                ("D".into(), "q \"x\" \n end".into()),
                ("E".into(), "".into()),
                ("F".into(), "multi\nline".into()),
            ]
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("NOEQUALS\n").is_err());
        assert!(parse("1BAD=x\n").is_err());
        assert!(parse("A='open\n").is_err());
    }
}
