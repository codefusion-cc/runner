//! The `${{ ... }}` expressions a `runs-on` can hold, as far as the Runner reads them: whole expressions over
//! repository variables, string literals and functions. Port of the Go `poolExpression`, plus a small evaluator.

use std::collections::BTreeMap;

/// The inside of a label that is one whole expression, `${{ inside }}`. As GitHub reads it, the expression ends at
/// the first `}}` outside a string literal, which must end the label.
fn whole(label: &str) -> Option<&str> {
    let rest = label.trim().strip_prefix("${{")?;
    let bytes = rest.as_bytes();
    let mut in_literal = false;
    let mut end = None;
    for i in 0..bytes.len() {
        match bytes[i] {
            // A doubled quote inside a literal is two toggles: out and in again.
            b'\'' => in_literal = !in_literal,
            b'}' if !in_literal && bytes.get(i + 1) == Some(&b'}') => {
                end = Some(i);
                break;
            }
            _ => {}
        }
    }
    let end = end?;
    let inner = &rest[..end];
    // The Go pattern's `.` does not match a newline, and two expressions in one label are not one.
    (end + 2 == rest.len() && !inner.contains('\n') && !without_literals(inner)?.contains("${{"))
        .then_some(inner)
}

/// `body` with every string literal (`'...'`, a doubled quote inside is one quote) replaced by `0`, or `None` when
/// a quote is left unclosed.
fn without_literals(body: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\'' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\'' {
                i += 1;
            }
            out.push_str(&body[start..i]);
            continue;
        }
        i += 1;
        loop {
            match bytes.get(i) {
                None => return None,
                Some(b'\'') if bytes.get(i + 1) == Some(&b'\'') => i += 2,
                Some(b'\'') => {
                    i += 1;
                    break;
                }
                Some(_) => i += 1,
            }
        }
        out.push('0');
    }
    Some(out)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Whether a label is one whole expression whose value can only come from repository variables and literals:
/// `${{ vars.CI_RUNNER || 'ubuntu-latest' }}`, `${{ toJSON(vars.CI_RUNNER_SMALL) }}`. Function names are allowed;
/// contexts other than `vars` (matrix, inputs, github, env, ...) are not.
pub(crate) fn is_pool_expression(label: &str) -> bool {
    let Some(inner) = whole(label) else { return false };
    let Some(body) = without_literals(inner) else {
        return false;
    };
    let bytes = body.as_bytes();
    let mut uses_vars = false;
    let mut i = 0;
    while i < bytes.len() {
        if !is_ident_start(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident(bytes[i]) {
            i += 1;
        }
        let name = body[start..i].to_ascii_lowercase();
        let before = body[..start].trim_end_matches([' ', '\t']);
        let after = body[i..].trim_start_matches([' ', '\t']);
        if before.ends_with('.') || (start > 0 && bytes[start - 1].is_ascii_digit()) {
            // a property (vars.CI_RUNNER) or a number's exponent (1e3)
        } else if after.starts_with('(') {
            // a function: toJSON, fromJSON, format
        } else if name == "vars" {
            uses_vars = true;
        } else if !matches!(name.as_str(), "true" | "false" | "null") {
            return false;
        }
    }
    uses_vars
}

/// The deepest nesting of parentheses and calls that is read: GitHub's own expressions are shallow, and a hostile one
/// must not exhaust the stack.
pub(crate) const MAX_DEPTH: usize = 32;

/// The longest text a value may grow to (`format`, `toJSON`) before the expression is not read: a few nested calls
/// could otherwise double a text into gigabytes.
pub(crate) const MAX_VALUE_BYTES: usize = 64 * 1024;

/// A value of an expression, as GitHub has them: what `fromJSON` can make, and the strings, booleans, numbers and
/// null of literals and variables.
pub(crate) type Value = serde_json::Value;

/// What a pool expression evaluates to, as labels: [`value`] of its inside, a string as one label (a JSON string,
/// `"cf-8g"` as `toJSON` makes it, unquoted) and a list of strings, which only `fromJSON` makes, as its labels.
/// `None` when the expression cannot be read, or its value is empty or anything else: a list of anything but
/// strings names none of these Actions runners.
pub(crate) fn evaluate(label: &str, vars: &BTreeMap<String, String>) -> Option<Vec<String>> {
    match value(whole(label)?, vars)? {
        Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| vec![unquote_json(text)])
        }
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(label) => Some(label),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// The value of the inside of a `${{ ... }}` when it is made of what this reader knows, as GitHub evaluates it:
/// string literals (`''` is a quote), `true`, `false`, `null` and numbers, `vars.NAME` and `vars['NAME']` (a name
/// is matched without regard to case, an unset one is empty), `a || b` (the first operand that is truthy, else the
/// last), parentheses, and the functions `format`, `toJSON` and `fromJSON`. `None` for anything else: another
/// context, operator or function, a `format` GitHub would refuse, nesting deeper than [`MAX_DEPTH`], a value longer
/// than [`MAX_VALUE_BYTES`].
///
/// One leniency: `fromJSON` of a text that is not JSON is that text, where GitHub fails the job. Read so, the job
/// asks for the scale set its author meant.
pub(crate) fn value(inner: &str, vars: &BTreeMap<String, String>) -> Option<Value> {
    let tokens = tokens(inner)?;
    let mut parser = Parser {
        tokens: &tokens,
        at: 0,
        vars,
    };
    let value = parser.or(0)?;
    (parser.at == tokens.len()).then_some(value)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Literal(String),
    Number(f64),
    Word(String),
    Open,
    Close,
    OpenIndex,
    CloseIndex,
    Comma,
    Dot,
    Or,
}

/// The tokens of an expression; `None` at a character this reader does not know, an operator other than `||`, or
/// a literal left open.
fn tokens(inner: &str) -> Option<Vec<Token>> {
    let bytes = inner.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let token = match b {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            b'\'' => {
                let mut text = Vec::new();
                i += 1;
                loop {
                    match bytes.get(i)? {
                        b'\'' if bytes.get(i + 1) == Some(&b'\'') => {
                            text.push(b'\'');
                            i += 2;
                        }
                        b'\'' => break,
                        other => {
                            text.push(*other);
                            i += 1;
                        }
                    }
                }
                i += 1;
                // Cut only at quotes, so the bytes between are whole characters.
                Token::Literal(String::from_utf8(text).ok()?)
            }
            b'(' | b')' | b'[' | b']' | b',' | b'.' => {
                i += 1;
                match b {
                    b'(' => Token::Open,
                    b')' => Token::Close,
                    b'[' => Token::OpenIndex,
                    b']' => Token::CloseIndex,
                    b',' => Token::Comma,
                    _ => Token::Dot,
                }
            }
            b'|' if bytes.get(i + 1) == Some(&b'|') => {
                i += 2;
                Token::Or
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                    // An exponent's sign: 1e-3, 1E+3.
                    if matches!(bytes[i], b'e' | b'E') && matches!(bytes.get(i + 1), Some(b'-' | b'+')) {
                        i += 1;
                    }
                    i += 1;
                }
                Token::Number(number(&inner[start..i])?)
            }
            b if is_ident_start(b) => {
                let start = i;
                while i < bytes.len() && is_ident(bytes[i]) {
                    i += 1;
                }
                Token::Word(inner[start..i].to_ascii_lowercase())
            }
            _ => return None,
        };
        out.push(token);
    }
    Some(out)
}

/// A number as GitHub writes them: decimal, with a fraction or an exponent, or hexadecimal (`0x10`).
fn number(text: &str) -> Option<f64> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok().map(|n| n as f64);
    }
    text.parse::<f64>().ok().filter(|n| n.is_finite())
}

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
    vars: &'a BTreeMap<String, String>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at)?.clone();
        self.at += 1;
        Some(token)
    }

    fn expect(&mut self, token: &Token) -> Option<()> {
        (self.next()? == *token).then_some(())
    }

    /// `a || b || ...`: the first operand that is truthy, else the last. Every operand is read, so one this reader
    /// cannot read makes the whole unread.
    fn or(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut chosen: Option<Value> = None;
        loop {
            let operand = self.primary(depth)?;
            if !chosen.as_ref().is_some_and(truthy) {
                chosen = Some(operand);
            }
            if self.peek() != Some(&Token::Or) {
                return chosen;
            }
            self.at += 1;
        }
    }

    fn primary(&mut self, depth: usize) -> Option<Value> {
        match self.next()? {
            Token::Literal(text) => Some(Value::String(text)),
            Token::Number(n) => number_value(n),
            Token::Open => {
                let value = self.or(depth + 1)?;
                self.expect(&Token::Close)?;
                Some(value)
            }
            Token::Word(word) => match word.as_str() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                "null" => Some(Value::Null),
                "vars" => self.variable(),
                _ if self.peek() == Some(&Token::Open) => {
                    self.at += 1;
                    let args = self.arguments(depth + 1)?;
                    call(&word, args)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// `.NAME` or `['NAME']` after `vars`; nothing may follow it but an operator, a comma or a parenthesis.
    fn variable(&mut self) -> Option<Value> {
        let name = match self.next()? {
            Token::Dot => match self.next()? {
                Token::Word(name) => name,
                _ => return None,
            },
            Token::OpenIndex => {
                let Token::Literal(name) = self.next()? else {
                    return None;
                };
                self.expect(&Token::CloseIndex)?;
                name
            }
            _ => return None,
        };
        if matches!(self.peek(), Some(Token::Dot | Token::OpenIndex)) {
            return None;
        }
        if name.is_empty() || !name.bytes().all(is_ident) {
            return None;
        }
        // GitHub compares variable names without regard to case.
        let found = self
            .vars
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        bounded(found)
    }

    /// The arguments of a call up to its `)`, which [`Parser::primary`] opened.
    fn arguments(&mut self, depth: usize) -> Option<Vec<Value>> {
        let mut args = Vec::new();
        if self.peek() == Some(&Token::Close) {
            self.at += 1;
            return Some(args);
        }
        loop {
            args.push(self.or(depth)?);
            match self.next()? {
                Token::Comma => {}
                Token::Close => return Some(args),
                _ => return None,
            }
        }
    }
}

fn call(function: &str, args: Vec<Value>) -> Option<Value> {
    let mut args = args.into_iter();
    match function {
        "tojson" => {
            let value = args.next()?;
            if args.next().is_some() {
                return None;
            }
            bounded(serde_json::to_string_pretty(&value).ok()?)
        }
        "fromjson" => {
            let value = args.next()?;
            if args.next().is_some() {
                return None;
            }
            // GitHub reads any value as its text first.
            let text = as_text(&value);
            Some(serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)))
        }
        "format" => {
            let template = as_text(&args.next()?);
            format(&template, &args.collect::<Vec<_>>())
        }
        _ => None,
    }
}

/// GitHub's `format`: `{N}` is the Nth argument after the template as text, `{{` and `}}` are braces. A brace
/// otherwise, an index that is no argument, or a specifier (`{0:x}`) is a template GitHub refuses.
fn format(template: &str, args: &[Value]) -> Option<Value> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        let brace = rest.as_bytes()[at];
        let after = &rest[at + 1..];
        if after.as_bytes().first() == Some(&brace) {
            out.push(char::from(brace));
            rest = &after[1..];
            continue;
        }
        if brace == b'}' {
            return None;
        }
        let close = after.find('}')?;
        let index = &after[..close];
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out.push_str(&as_text(args.get(index.parse::<usize>().ok()?)?));
        if out.len() > MAX_VALUE_BYTES {
            return None;
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    bounded(out)
}

/// A value as `format` writes it: text as it is, `true`, `false`, nothing for null, numbers without a fraction
/// when they have none; GitHub writes a list or an object by its kind.
fn as_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        Value::String(s) => s.clone(),
        Value::Array(_) => "Array".into(),
        Value::Object(_) => "Object".into(),
    }
}

fn number_value(n: f64) -> Option<Value> {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return Some(Value::from(n as i64));
    }
    serde_json::Number::from_f64(n).map(Value::Number)
}

/// A text as a value, or `None` past [`MAX_VALUE_BYTES`].
fn bounded(text: String) -> Option<Value> {
    (text.len() <= MAX_VALUE_BYTES).then_some(Value::String(text))
}

/// GitHub's truthiness: `false`, `0`, `''` and `null` are false, everything else true.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn unquote_json(value: &str) -> String {
    serde_saphyr::from_str::<String>(value)
        .ok()
        .filter(|_| value.starts_with('"'))
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
mod tests {
    //! The reader against GitHub's own rules for expressions: `||` gives the first operand that is truthy, `( )`
    //! groups, `format` fills `{0}`..`{n}` and reads `{{` and `}}` as braces, `''` is a quote inside a literal.

    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn vars() -> BTreeMap<String, String> {
        [
            ("A", "cf-2g"),
            ("EMPTY", ""),
            ("LIST", r#"["cf-8g","cf-macos"]"#),
            ("B", "x y"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn of(inner: &str) -> Option<Value> {
        value(inner, &vars())
    }

    fn text(inner: &str) -> Option<String> {
        match of(inner) {
            Some(Value::String(s)) => Some(s),
            other => panic!("{inner}: not a string but {other:?}"),
        }
    }

    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    /// GitHub's truthiness as its documentation lists it, not as the code reads it: false, 0, -0, '' and null are
    /// false (NaN is no JSON value); everything else is true.
    fn truthy(value: &Value) -> bool {
        ![
            json!(false),
            json!(0),
            json!(0.0),
            json!(-0.0),
            json!(""),
            Value::Null,
        ]
        .contains(value)
    }

    #[test]
    fn format_fills_its_arguments_and_reads_doubled_braces_as_braces() {
        assert_eq!(text("format('{0}', 'x')").unwrap(), "x");
        assert_eq!(text("format('{0}{1}{0}', 'a', 'b')").unwrap(), "aba");
        assert_eq!(text("format('{1}-{0}', vars.A, 'z')").unwrap(), "z-cf-2g");
        assert_eq!(text("format('{{0}}', 'x')").unwrap(), "{0}");
        assert_eq!(text("format('{{{0}}}', 'x')").unwrap(), "{x}");
        assert_eq!(text("format('}}{{')").unwrap(), "}{");
        assert_eq!(text("format('no braces')").unwrap(), "no braces");
        assert_eq!(text("format('it''s {0}', 'x''y')").unwrap(), "it's x'y");
        assert_eq!(text("format('\"{0}\"', vars.A)").unwrap(), "\"cf-2g\"");
        assert_eq!(
            text("format('[{0}]', vars.MISSING)").unwrap(),
            "[]",
            "a variable nobody set is empty"
        );
        assert_eq!(text("format('{0}|{1}|{2}', true, null, 3)").unwrap(), "true||3");
        assert_eq!(text("format('{0}', 2.5)").unwrap(), "2.5");
        assert_eq!(
            text("FORMAT('{0}', 'x')").unwrap(),
            "x",
            "function names ignore case"
        );
        assert_eq!(
            text("format(vars.A)").unwrap(),
            "cf-2g",
            "the format string may be any string"
        );
        assert_eq!(text("format('{0}', format('<{0}>', 'x'))").unwrap(), "<x>");
        assert_eq!(text("format('{10}', 0,1,2,3,4,5,6,7,8,9,'ten')").unwrap(), "ten");
    }

    #[test]
    fn a_format_github_refuses_is_not_read() {
        for inner in [
            "format('{1}', 'a')",
            "format('{0}')",
            "format('{')",
            "format('}')",
            "format('{0')",
            "format('a}b')",
            "format('{a}', 'x')",
            "format('{ 0}', 'x')",
            "format('{-1}', 'x')",
            "format('{0:x}', 'x')",
            "format()",
            "format('{0}', 'x'",
            "format('{0}' 'x')",
            "format('{0}',, 'x')",
        ] {
            assert_eq!(of(inner), None, "{inner}");
        }
    }

    #[test]
    fn parentheses_group_and_change_nothing_else() {
        assert_eq!(text("(vars.A)").unwrap(), "cf-2g");
        assert_eq!(text("((vars.EMPTY)) || ('z')").unwrap(), "z");
        assert_eq!(text("(vars.EMPTY || vars.A) || 'z'").unwrap(), "cf-2g");
        assert_eq!(of("fromJSON((vars.LIST))"), Some(json!(["cf-8g", "cf-macos"])));
        assert_eq!(
            of(&format!("{}vars.A{}", "(".repeat(10), ")".repeat(10))),
            Some(json!("cf-2g"))
        );
        for broken in [
            "(vars.A",
            "vars.A)",
            "()",
            "(vars.A))",
            ")(",
            "(vars.A || )",
            "|| vars.A",
        ] {
            assert_eq!(of(broken), None, "{broken}");
        }
    }

    #[test]
    fn what_the_reader_does_not_do_is_still_not_read() {
        for inner in [
            "vars.A && vars.B",
            "!vars.A",
            "vars.A == 'x'",
            "vars.A != 'x'",
            "vars.A < 'x'",
            "contains(vars.A, 'x')",
            "github.ref",
            "vars.A.b",
            "vars",
            "vars[0]",
            "vars['A'",
            "'unclosed",
            "vars.A vars.B",
            "join(fromJSON(vars.LIST), ',')",
        ] {
            assert_eq!(of(inner), None, "{inner}");
        }
    }

    #[test]
    fn variables_literals_and_json() {
        assert_eq!(text("vars.A").unwrap(), "cf-2g");
        assert_eq!(text("VARS.a").unwrap(), "cf-2g", "names ignore case");
        assert_eq!(text("vars['A']").unwrap(), "cf-2g");
        assert_eq!(text("vars.MISSING").unwrap(), "");
        assert_eq!(of("true"), Some(json!(true)));
        assert_eq!(of("FALSE"), Some(json!(false)));
        assert_eq!(of("null"), Some(Value::Null));
        assert_eq!(of("0x10"), Some(json!(16)));
        assert_eq!(of("1e3"), Some(json!(1000)));
        assert_eq!(of("toJSON(vars.A)"), Some(json!("\"cf-2g\"")));
        assert_eq!(
            of("fromJSON(toJSON(vars.LIST))"),
            Some(json!(r#"["cf-8g","cf-macos"]"#))
        );
        assert_eq!(of("fromJSON(vars.LIST)"), Some(json!(["cf-8g", "cf-macos"])));
    }

    #[test]
    fn the_runs_on_of_a_job_that_names_its_size_in_a_variable_or_a_format_is_read() {
        let labels = |label: &str, vars: &[(&str, &str)]| {
            let vars = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            evaluate(label, &vars)
        };
        let heavy = "${{ fromJSON(vars.HEAVY || format('\"{0}\"', vars.RUNNER || 'ubuntu-latest')) }}";
        assert_eq!(
            labels(heavy, &[("RUNNER", "cf-2g")]),
            Some(vec!["cf-2g".to_string()])
        );
        assert_eq!(labels(heavy, &[]), Some(vec!["ubuntu-latest".to_string()]));
        assert_eq!(
            labels(
                heavy,
                &[("HEAVY", r#"["cf-8g","cf-macos"]"#), ("RUNNER", "cf-2g")]
            ),
            Some(vec!["cf-8g".to_string(), "cf-macos".to_string()])
        );
        // A `}}` inside a literal does not end the expression, as GitHub reads it.
        assert_eq!(
            labels("${{ format('{{{0}}}', vars.R) }}", &[("R", "x")]),
            Some(vec!["{x}".to_string()])
        );
        assert_eq!(labels("${{ vars.R }}-${{ vars.R }}", &[("R", "x")]), None);
        assert!(is_pool_expression("${{ format('{{{0}}}', vars.R) }}"));
        assert!(!is_pool_expression("${{ vars.R }}-${{ vars.R }}"));
    }

    #[test]
    fn nesting_deeper_than_the_reader_goes_is_not_read_and_does_not_overflow() {
        let deep = |n: usize| format!("{}vars.A{}", "(".repeat(n), ")".repeat(n));
        assert_eq!(of(&deep(MAX_DEPTH - 1)), Some(json!("cf-2g")));
        assert_eq!(of(&deep(MAX_DEPTH + 1)), None);
        assert_eq!(of(&deep(100_000)), None);
        let calls = format!("{}vars.A{}", "toJSON(".repeat(100_000), ")".repeat(100_000));
        assert_eq!(of(&calls), None);
    }

    #[test]
    fn a_value_that_would_grow_past_the_bound_is_not_read() {
        // Each level doubles the text: 2^24 times 8 bytes, were it built.
        let mut expression = "'xxxxxxxx'".to_string();
        for _ in 0..24 {
            expression = format!("format('{{0}}{{0}}', {expression})");
        }
        let began = std::time::Instant::now();
        assert_eq!(of(&expression), None);
        assert!(
            began.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
        // Just under the bound it is read.
        let mut expression = "'x'".to_string();
        let mut len = 1;
        while len * 2 <= MAX_VALUE_BYTES {
            expression = format!("format('{{0}}{{0}}', {expression})");
            len *= 2;
        }
        assert_eq!(text(&expression).map(|s| s.len()), Some(len));
    }

    fn leaf() -> impl Strategy<Value = String> {
        prop_oneof![
            "\\PC{0,12}".prop_map(|s| quote(&s)),
            prop::sample::select(vec![
                "vars.A",
                "vars.EMPTY",
                "vars.LIST",
                "vars['B']",
                "VARS.a",
                "vars.MISSING",
                "true",
                "false",
                "null",
                "0",
                "1",
                "2.5",
            ])
            .prop_map(String::from),
        ]
    }

    /// Expressions the reader reads: every operator and function it knows, nested.
    fn expression() -> impl Strategy<Value = String> {
        leaf().prop_recursive(4, 24, 3, |inner| {
            prop_oneof![
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} || {b}")),
                inner.clone().prop_map(|a| format!("({a})")),
                inner.clone().prop_map(|a| format!("toJSON({a})")),
                inner.clone().prop_map(|a| format!("fromJSON({a})")),
                (inner.clone(), inner.clone())
                    .prop_map(|(a, b)| format!("format('{{1}}-{{{{{{0}}}}}}', {a}, {b})")),
            ]
        })
    }

    proptest! {
        #[test]
        fn nothing_makes_the_reader_panic(inner in "[ -~\u{00e9}\u{1F600}]{0,80}") {
            let _ = value(&inner, &vars());
            let _ = evaluate(&format!("${{{{ {inner} }}}}"), &vars());
        }

        #[test]
        fn nothing_built_of_its_own_pieces_makes_it_panic(
            pieces in prop::collection::vec(prop::sample::select(vec![
                "(", ")", "||", ",", "'", "''", "{", "}", "{{", "}}", "{0}", "{1}", "vars.A", "vars[", "]", "format(",
                "toJSON(", "fromJSON(", "&&", "!", " ", "1", ".", "\u{1F600}",
            ]), 0..40)
        ) {
            let _ = value(&pieces.concat(), &vars());
        }

        #[test]
        fn format_of_a_literal_alone_is_the_literal(x in "\\PC{0,40}") {
            prop_assert_eq!(of(&format!("format('{{0}}', {})", quote(&x))), Some(Value::String(x.clone())));
            prop_assert_eq!(of(&quote(&x)), Some(Value::String(x)));
        }

        #[test]
        fn doubled_braces_are_one_brace_whatever_surrounds_them(x in "[a-z ]{0,10}", y in "[a-z ]{0,10}") {
            let template = format!("{x}{{{{{y}}}}}{{0}}");
            prop_assert_eq!(
                of(&format!("format({}, 'v')", quote(&template))),
                Some(Value::String(format!("{x}{{{y}}}v")))
            );
        }

        #[test]
        fn parentheses_around_an_expression_change_nothing(a in expression(), depth in 1usize..6) {
            let plain = of(&a);
            prop_assert!(plain.is_some(), "{} is read", a);
            let wrapped = format!("{}{a}{}", "(".repeat(depth), ")".repeat(depth));
            prop_assert_eq!(of(&wrapped), plain.clone());
            // And once more around what is wrapped already: the same again.
            prop_assert_eq!(of(&format!("({wrapped})")), plain);
        }

        #[test]
        fn or_gives_the_first_operand_that_is_not_empty(a in expression(), b in expression()) {
            let (x, y) = (of(&a).expect("a is read"), of(&b).expect("b is read"));
            let expected = if truthy(&x) { x } else { y };
            prop_assert_eq!(of(&format!("({a}) || ({b})")), Some(expected));
        }
    }
}
