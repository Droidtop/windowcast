//! What CreateProcessAsUserW takes instead of an argument list and a map
//! of variables: one command line, and one environment block. Plain
//! functions, so their tests run on every platform.

/// The command line Windows programs split back into `argv` (the rules of
/// the C runtime and CommandLineToArgvW). The program name is only quoted:
/// those rules read it up to the next quote or space, with no escapes.
pub fn join(argv: &[String]) -> String {
    let mut line = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        if i == 0 {
            if arg.is_empty() || arg.contains([' ', '\t']) {
                line.push('"');
                line.push_str(arg);
                line.push('"');
            } else {
                line.push_str(arg);
            }
        } else {
            quote(&mut line, arg);
        }
    }
    line
}

fn quote(line: &mut String, arg: &str) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Each backslash before a quote is doubled, and the quote
                // itself escaped.
                line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                line.push('"');
                backslashes = 0;
            }
            _ => {
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push(c);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote are doubled too.
    line.extend(std::iter::repeat_n('\\', backslashes * 2));
    line.push('"');
}

/// The variables of an environment block (`NAME=value` strings, each
/// ended by a NUL, the block by another NUL).
pub fn parse_env_block(block: &[u16]) -> Vec<(String, String)> {
    block
        .split(|&unit| unit == 0)
        .take_while(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let entry = String::from_utf16_lossy(entry);
            // A name may start with '=' (the per-drive directories, "=C:").
            let split = entry[1..].find('=')? + 1;
            Some((entry[..split].to_owned(), entry[split + 1..].to_owned()))
        })
        .collect()
}

/// An environment block of `base` with `extra` set on top, names compared
/// as Windows does (without case), sorted as Windows keeps them.
pub fn env_block(base: Vec<(String, String)>, extra: &[(String, String)]) -> Vec<u16> {
    let mut vars = base;
    for (name, value) in extra {
        vars.retain(|(have, _)| !have.eq_ignore_ascii_case(name));
        vars.push((name.clone(), value.clone()));
    }
    vars.sort_by_key(|(name, _)| name.to_uppercase());
    let mut block: Vec<u16> = vars
        .iter()
        .flat_map(|(name, value)| {
            format!("{name}={value}")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>()
        })
        .collect();
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How the C runtime splits a command line (arguments after the
    /// program name), the reading `join` writes for.
    fn split(line: &str) -> Vec<String> {
        let mut chars = line.chars().peekable();
        let mut args = Vec::new();
        // The program name: up to a space outside quotes, no escapes.
        let mut program = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                quoted = !quoted;
            } else if (c == ' ' || c == '\t') && !quoted {
                break;
            } else {
                program.push(c);
            }
            chars.next();
        }
        args.push(program);
        loop {
            while chars.peek().is_some_and(|&c| c == ' ' || c == '\t') {
                chars.next();
            }
            if chars.peek().is_none() {
                return args;
            }
            let mut arg = String::new();
            let mut quoted = false;
            loop {
                let mut backslashes = 0;
                while chars.peek() == Some(&'\\') {
                    backslashes += 1;
                    chars.next();
                }
                match chars.peek() {
                    Some('"') => {
                        arg.extend(std::iter::repeat_n('\\', backslashes / 2));
                        chars.next();
                        if backslashes % 2 == 1 {
                            arg.push('"');
                        } else if quoted && chars.peek() == Some(&'"') {
                            arg.push('"');
                            chars.next();
                        } else {
                            quoted = !quoted;
                        }
                    }
                    Some(&c) if (c == ' ' || c == '\t') && !quoted => {
                        arg.extend(std::iter::repeat_n('\\', backslashes));
                        break;
                    }
                    Some(&c) => {
                        arg.extend(std::iter::repeat_n('\\', backslashes));
                        arg.push(c);
                        chars.next();
                    }
                    None => {
                        arg.extend(std::iter::repeat_n('\\', backslashes));
                        break;
                    }
                }
            }
            args.push(arg);
        }
    }

    fn strings(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_arguments_stay_as_they_are() {
        let argv = strings(&["cmd.exe", "/c", "echo", "hi"]);
        assert_eq!(join(&argv), "cmd.exe /c echo hi");
    }

    #[test]
    fn quoting_follows_the_c_runtime() {
        for (arg, line) in [
            ("a b", r#""a b""#),
            ("", r#""""#),
            (r#"a"b"#, r#""a\"b""#),
            (r"a\b", r"a\b"),
            (r"C:\with space\", r#""C:\with space\\""#),
            (r#"a\"b c"#, r#""a\\\"b c""#),
        ] {
            assert_eq!(join(&strings(&["p", arg])), format!("p {line}"), "{arg:?}");
        }
    }

    #[test]
    fn every_argument_comes_back_as_it_went() {
        let pieces = ["", " ", "\\", "\"", "a", "\t", "\\\\", "b c", "\\\""];
        for a in pieces {
            for b in pieces {
                for c in pieces {
                    let arg = format!("{a}{b}{c}");
                    let argv = vec![
                        r"C:\Program Files\x.exe".to_owned(),
                        arg.clone(),
                        "end".into(),
                    ];
                    assert_eq!(split(&join(&argv)), argv, "{arg:?}");
                }
            }
        }
    }

    fn block(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn an_environment_block_reads_back() {
        let vars = parse_env_block(&block("=C:=C:\\\0PATH=C:\\a;C:\\b\0X=1=2\0\0"));
        assert_eq!(
            vars,
            [
                ("=C:".to_owned(), "C:\\".to_owned()),
                ("PATH".into(), "C:\\a;C:\\b".into()),
                ("X".into(), "1=2".into()),
            ]
        );
    }

    #[test]
    fn extra_variables_replace_the_users_own_without_case() {
        let base = vec![
            ("Path".to_owned(), "C:\\user".to_owned()),
            ("USERNAME".into(), "me".into()),
        ];
        let extra = [
            ("PATH".to_owned(), "C:\\mine".to_owned()),
            ("A".into(), "1".into()),
        ];
        let block = env_block(base, &extra);
        assert_eq!(block, self::block("A=1\0PATH=C:\\mine\0USERNAME=me\0\0"));
        assert_eq!(parse_env_block(&block).len(), 3);
    }

    #[test]
    fn an_empty_environment_is_two_nuls() {
        assert_eq!(env_block(Vec::new(), &[]), [0, 0]);
    }
}
