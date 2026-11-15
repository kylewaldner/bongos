//! Check 8, `secrets`: no field the schema marks secret holds a literal, and no secret pattern
//! (the setting `project_repository.secret_patterns`) matches changed text. A refusal names the
//! file, line and pattern, never the matched text.

use regex::Regex;

use super::outcome::CheckOutcome;

/// Compiles the secret patterns; an invalid one is named in the error.
pub fn compile_secret_patterns(patterns: &[String]) -> Result<Vec<Regex>, String> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|error| format!("secret pattern {pattern}: {error}"))
        })
        .collect()
}

/// Check 8 over the changed text files, `(path, text)`. `secret_keys` are the dotted keys the
/// schema marks secret (such as `linear.token`); in a `.toml` file such a key holds no value
/// (secrets are read from the machine, never from the repository).
pub fn secrets<'a>(
    texts: impl IntoIterator<Item = (&'a str, &'a str)>,
    patterns: &[Regex],
    secret_keys: &[String],
) -> CheckOutcome {
    let mut refusals = Vec::new();
    for (path, text) in texts {
        for (number, line) in text.lines().enumerate() {
            for pattern in patterns {
                if pattern.is_match(line) {
                    refusals.push(format!(
                        "{path}:{}: matches the secret pattern {}",
                        number + 1,
                        pattern.as_str()
                    ));
                }
            }
        }
        if !path.ends_with(".toml") || secret_keys.is_empty() {
            continue;
        }
        let Ok(table) = text.parse::<toml::Table>() else {
            continue;
        };
        for key in secret_keys {
            if lookup(&table, key).is_some_and(holds_literal) {
                refusals.push(format!(
                    "{path}: {key} is secret and may not hold a value in the repository"
                ));
            }
        }
    }
    CheckOutcome::from_refusals(refusals)
}

fn lookup<'t>(table: &'t toml::Table, dotted: &str) -> Option<&'t toml::Value> {
    let mut parts = dotted.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.as_table()?.get(part)?;
    }
    Some(value)
}

fn holds_literal(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(text) => !text.is_empty(),
        _ => true,
    }
}
