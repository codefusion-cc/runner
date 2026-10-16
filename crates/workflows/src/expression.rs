//! The `${{ ... }}` expressions a `runs-on` can hold, as far as the Runner reads them: whole expressions over
//! repository variables, string literals and functions. Port of the Go `poolExpression`, plus a small evaluator.

use std::collections::BTreeMap;

/// The inside of a label that is one whole expression, `${{ inside }}`.
fn whole(label: &str) -> Option<&str> {
    let inner = label.trim().strip_prefix("${{")?.strip_suffix("}}")?;
    // The Go pattern's `.` does not match a newline, and two expressions in one label are not one.
    (!inner.contains("${{") && !inner.contains("}}") && !inner.contains('\n')).then_some(inner)
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

/// What a pool expression evaluates to, when it is a chain of `||` over `vars.NAME`, `vars['NAME']` and string
/// literals, optionally wrapped in `toJSON(...)` / `fromJSON(...)`: the labels of the first operand that is not
/// empty. `None` when the expression is anything else or every operand is empty. A value that is a JSON string
/// (`"cf-8g"`, as `toJSON` makes) comes back unquoted. Under `fromJSON` with no `toJSON` anywhere a JSON list of
/// strings (`["cf-8g","cf-macos"]`) is every label in it, as GitHub reads it; a list of anything else is one label, which
/// names none of these Actions runners.
pub(crate) fn evaluate(label: &str, vars: &BTreeMap<String, String>) -> Option<Vec<String>> {
    let mut inner = whole(label)?.trim();
    let (mut from_json, mut to_json) = (false, false);
    while let Some(function) = ["tojson(", "fromjson("].into_iter().find(|f| {
        inner
            .get(..f.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(f))
    }) {
        if !inner.ends_with(')') {
            break;
        }
        from_json |= function == "fromjson(";
        to_json |= function == "tojson(";
        inner = inner[inner.find('(')? + 1..inner.len() - 1].trim();
    }
    for operand in split_or(inner)? {
        let value = operand_value(operand.trim(), vars)?;
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        // Any toJSON makes a string again, which a fromJSON around it only parses back.
        if from_json
            && !to_json
            && let Ok(labels) = serde_json::from_str::<Vec<String>>(value)
        {
            return Some(labels);
        }
        return Some(vec![unquote_json(value)]);
    }
    None
}

/// Splits on `||` outside string literals; `None` when there is any other operator or a parenthesis (anything this
/// evaluator does not do).
fn split_or(s: &str) -> Option<Vec<&str>> {
    let bytes = s.as_bytes();
    let (mut parts, mut start, mut i) = (vec![], 0, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                loop {
                    match bytes.get(i)? {
                        b'\'' if bytes.get(i + 1) == Some(&b'\'') => i += 2,
                        b'\'' => break,
                        _ => i += 1,
                    }
                }
                i += 1;
            }
            b'|' if bytes.get(i + 1) == Some(&b'|') => {
                parts.push(&s[start..i]);
                i += 2;
                start = i;
            }
            b'|' | b'&' | b'(' | b')' | b'!' | b'=' | b'<' | b'>' => return None,
            _ => i += 1,
        }
    }
    parts.push(&s[start..]);
    Some(parts)
}

fn operand_value(operand: &str, vars: &BTreeMap<String, String>) -> Option<String> {
    if let Some(literal) = operand.strip_prefix('\'').and_then(|o| o.strip_suffix('\'')) {
        return Some(literal.replace("''", "'"));
    }
    let name = if let Some(rest) = operand.strip_prefix("vars.") {
        rest.trim()
    } else {
        let rest = operand.strip_prefix("vars[")?.strip_suffix(']')?.trim();
        rest.strip_prefix('\'')?.strip_suffix('\'')?
    };
    if name.is_empty() || !name.bytes().all(is_ident) {
        return None;
    }
    // GitHub compares variable names without regard to case.
    Some(
        vars.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default(),
    )
}

fn unquote_json(value: &str) -> String {
    serde_saphyr::from_str::<String>(value)
        .ok()
        .filter(|_| value.starts_with('"'))
        .unwrap_or_else(|| value.to_string())
}
