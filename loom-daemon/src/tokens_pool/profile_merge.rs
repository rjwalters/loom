//! Key-merge engines for pooled-profile settings documents (issue #8672).
//!
//! Both engines answer the same question, one key at a time: *may Loom write
//! the default profile's value here?* The answer is yes only when the key is
//! not on the provider's denylist **and** the pooled profile either has no
//! value there yet or still holds exactly what
//! [`super::profile_ledger::ProfileLedger`] says Loom last wrote. Anything
//! else is the operator's, and stays theirs.
//!
//! Two properties are load-bearing and are worth stating explicitly:
//!
//! 1. **Recursion is unconditional.** A table is never copied wholesale, even
//!    into a pooled profile that has no such table at all, because a denied
//!    key can sit at any depth (`hooks.state."<id>".trusted_hash`). Walking to
//!    the leaves is what makes the denylist total rather than top-level.
//! 2. **An inline table or array-of-tables that *contains* a denied key is
//!    skipped whole.** Splicing a denied key out of a composite value would
//!    hand the pooled profile a half-value the operator never wrote in either
//!    place; refusing it is the conservative direction, and it keeps "a
//!    denylisted key is never written" true without exception.
//!
//! TOML goes through `toml_edit`'s document model, so every key Loom does not
//! own — comments, ordering and formatting included — survives byte-for-byte.

use anyhow::{Context, Result};
use serde_json::Value as Json;
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

use super::profile_ledger::ProfileLedger;
use super::profile_sharing::ProviderProfileRules;

/// What a merge did, in terms the caller can report and record.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// The re-rendered document, present only when something changed.
    pub rendered: Option<String>,
    /// Keys written by this merge: `(dotted path, canonical value)`.
    pub written: Vec<(String, String)>,
    /// Keys already carrying exactly what Loom would have written. Recorded
    /// in the ledger all the same, so a profile provisioned before the ledger
    /// existed (or by a ledger-less path) becomes managed on the next run
    /// without a spurious rewrite.
    pub unchanged: Vec<(String, String)>,
    /// Keys left alone because the profile's value is not Loom's.
    pub preserved: Vec<String>,
    /// Keys refused by the provider's denylist. Never written, and never
    /// carried into the ledger.
    pub denied: Vec<String>,
}

impl MergeOutcome {
    #[must_use]
    pub fn changed(&self) -> bool {
        self.rendered.is_some()
    }
}

// ===========================================================================
// TOML
// ===========================================================================

/// Merge `source_text`'s keys into `target_text` under `rules`.
pub fn merge_toml(
    rules: &ProviderProfileRules,
    file_label: &str,
    source_text: &str,
    target_text: &str,
    ledger: &ProfileLedger,
) -> Result<MergeOutcome> {
    let source: DocumentMut = source_text
        .parse()
        .with_context(|| format!("the default profile's {file_label} is not valid TOML"))?;
    let mut target: DocumentMut = if target_text.trim().is_empty() {
        DocumentMut::new()
    } else {
        target_text.parse().with_context(|| {
            format!("this profile's {file_label} is not valid TOML; refusing to rewrite it")
        })?
    };

    let mut outcome = MergeOutcome::default();
    let mut writes: Vec<(Vec<String>, Item)> = Vec::new();
    plan_toml(
        rules,
        file_label,
        ledger,
        source.as_table(),
        Some(target.as_table()),
        &mut Vec::new(),
        &mut writes,
        &mut outcome,
    );
    if !writes.is_empty() {
        for (path, item) in writes {
            insert_toml_path(target.as_table_mut(), &path, item);
        }
        outcome.rendered = Some(target.to_string());
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn plan_toml(
    rules: &ProviderProfileRules,
    file_label: &str,
    ledger: &ProfileLedger,
    source: &Table,
    target: Option<&Table>,
    prefix: &mut Vec<String>,
    writes: &mut Vec<(Vec<String>, Item)>,
    outcome: &mut MergeOutcome,
) {
    for (key, source_item) in source.iter() {
        prefix.push(key.to_string());
        let dotted = prefix.join(".");
        if rules.is_denied_key(&dotted) {
            outcome.denied.push(dotted);
            prefix.pop();
            continue;
        }
        match source_item {
            Item::None => {}
            Item::Table(source_table) => match target.and_then(|t| t.get(key)) {
                Some(Item::Table(target_table)) => plan_toml(
                    rules,
                    file_label,
                    ledger,
                    source_table,
                    Some(target_table),
                    prefix,
                    writes,
                    outcome,
                ),
                None => plan_toml(
                    rules,
                    file_label,
                    ledger,
                    source_table,
                    None,
                    prefix,
                    writes,
                    outcome,
                ),
                // The profile holds a non-table where the default holds a
                // table. Loom has no safe merge for that shape clash.
                Some(_) => outcome.preserved.push(dotted),
            },
            leaf => {
                if toml_hides_denied_key(rules, prefix, leaf) {
                    outcome.denied.push(dotted);
                } else {
                    plan_leaf(
                        file_label,
                        ledger,
                        &dotted,
                        canonical_json(&toml_item_to_json(leaf)),
                        target
                            .and_then(|t| t.get(key))
                            .map(|item| canonical_json(&toml_item_to_json(item))),
                        || (prefix.clone(), leaf.clone()),
                        writes,
                        outcome,
                    );
                }
            }
        }
        prefix.pop();
    }
}

/// Insert `item` at `path`, creating intermediate (implicit) tables.
fn insert_toml_path(table: &mut Table, path: &[String], item: Item) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        table.insert(head, item);
        return;
    }
    let entry = table.entry(head).or_insert_with(|| {
        let mut created = Table::new();
        created.set_implicit(true);
        Item::Table(created)
    });
    if let Item::Table(nested) = entry {
        insert_toml_path(nested, rest, item);
    }
}

/// `true` when a composite TOML leaf carries a denied key anywhere inside it.
fn toml_hides_denied_key(
    rules: &ProviderProfileRules,
    prefix: &mut Vec<String>,
    item: &Item,
) -> bool {
    match item {
        Item::Value(TomlValue::InlineTable(inline)) => inline.iter().any(|(key, value)| {
            prefix.push(key.to_string());
            let hit = rules.is_denied_key(&prefix.join("."))
                || toml_hides_denied_key(rules, prefix, &Item::Value(value.clone()));
            prefix.pop();
            hit
        }),
        Item::Value(TomlValue::Array(array)) => array
            .iter()
            .any(|value| toml_hides_denied_key(rules, prefix, &Item::Value(value.clone()))),
        Item::ArrayOfTables(tables) => tables.iter().any(|table| {
            table.iter().any(|(key, nested)| {
                prefix.push(key.to_string());
                let hit = rules.is_denied_key(&prefix.join("."))
                    || toml_hides_denied_key(rules, prefix, nested);
                prefix.pop();
                hit
            })
        }),
        _ => false,
    }
}

// ===========================================================================
// JSON
// ===========================================================================

/// Merge `source_text`'s keys into `target_text` under `rules`.
pub fn merge_json(
    rules: &ProviderProfileRules,
    file_label: &str,
    source_text: &str,
    target_text: &str,
    ledger: &ProfileLedger,
) -> Result<MergeOutcome> {
    let source: Json = serde_json::from_str(source_text)
        .with_context(|| format!("the default profile's {file_label} is not valid JSON"))?;
    let mut target: Json = if target_text.trim().is_empty() {
        Json::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(target_text).with_context(|| {
            format!("this profile's {file_label} is not valid JSON; refusing to rewrite it")
        })?
    };
    let (Json::Object(source_map), Json::Object(_)) = (&source, &target) else {
        anyhow::bail!("{file_label} must be a JSON object in both profiles to be key-merged");
    };

    let mut outcome = MergeOutcome::default();
    let mut writes: Vec<(Vec<String>, Json)> = Vec::new();
    {
        let target_map = target.as_object().expect("checked above");
        plan_json(
            rules,
            file_label,
            ledger,
            source_map,
            Some(target_map),
            &mut Vec::new(),
            &mut writes,
            &mut outcome,
        );
    }
    if !writes.is_empty() {
        let target_map = target.as_object_mut().expect("checked above");
        for (path, value) in writes {
            insert_json_path(target_map, &path, value);
        }
        let mut rendered = serde_json::to_string_pretty(&target)?;
        rendered.push('\n');
        outcome.rendered = Some(rendered);
    }
    Ok(outcome)
}

type JsonMap = serde_json::Map<String, Json>;

#[allow(clippy::too_many_arguments)]
fn plan_json(
    rules: &ProviderProfileRules,
    file_label: &str,
    ledger: &ProfileLedger,
    source: &JsonMap,
    target: Option<&JsonMap>,
    prefix: &mut Vec<String>,
    writes: &mut Vec<(Vec<String>, Json)>,
    outcome: &mut MergeOutcome,
) {
    for (key, source_value) in source {
        prefix.push(key.clone());
        let dotted = prefix.join(".");
        if rules.is_denied_key(&dotted) {
            outcome.denied.push(dotted);
            prefix.pop();
            continue;
        }
        match source_value {
            Json::Object(source_map) => match target.and_then(|t| t.get(key)) {
                Some(Json::Object(target_map)) => plan_json(
                    rules,
                    file_label,
                    ledger,
                    source_map,
                    Some(target_map),
                    prefix,
                    writes,
                    outcome,
                ),
                None => {
                    plan_json(rules, file_label, ledger, source_map, None, prefix, writes, outcome)
                }
                Some(_) => outcome.preserved.push(dotted),
            },
            leaf => {
                if json_hides_denied_key(rules, prefix, leaf) {
                    outcome.denied.push(dotted);
                } else {
                    plan_leaf(
                        file_label,
                        ledger,
                        &dotted,
                        canonical_json(leaf),
                        target.and_then(|t| t.get(key)).map(canonical_json),
                        || (prefix.clone(), leaf.clone()),
                        writes,
                        outcome,
                    );
                }
            }
        }
        prefix.pop();
    }
}

fn insert_json_path(map: &mut JsonMap, path: &[String], value: Json) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        map.insert(head.clone(), value);
        return;
    }
    let entry = map
        .entry(head.clone())
        .or_insert_with(|| Json::Object(JsonMap::new()));
    if let Some(nested) = entry.as_object_mut() {
        insert_json_path(nested, rest, value);
    }
}

fn json_hides_denied_key(
    rules: &ProviderProfileRules,
    prefix: &mut Vec<String>,
    value: &Json,
) -> bool {
    match value {
        Json::Object(map) => map.iter().any(|(key, nested)| {
            prefix.push(key.clone());
            let hit = rules.is_denied_key(&prefix.join("."))
                || json_hides_denied_key(rules, prefix, nested);
            prefix.pop();
            hit
        }),
        Json::Array(items) => items
            .iter()
            .any(|item| json_hides_denied_key(rules, prefix, item)),
        _ => false,
    }
}

// ===========================================================================
// shared
// ===========================================================================

/// The ownership decision, identical for both document formats.
#[allow(clippy::too_many_arguments)]
fn plan_leaf<T>(
    file_label: &str,
    ledger: &ProfileLedger,
    dotted: &str,
    source_canonical: String,
    target_canonical: Option<String>,
    make_write: impl FnOnce() -> (Vec<String>, T),
    writes: &mut Vec<(Vec<String>, T)>,
    outcome: &mut MergeOutcome,
) {
    match target_canonical {
        // Nothing there yet — the blank-install case this whole feature exists
        // for.
        None => {
            writes.push(make_write());
            outcome.written.push((dotted.to_string(), source_canonical));
        }
        Some(current) => {
            if ledger.merged_value(file_label, dotted) != Some(current.as_str()) {
                // Either a human edited it, or it predates Loom's management
                // of this profile. Both are theirs.
                outcome.preserved.push(dotted.to_string());
            } else if current == source_canonical {
                outcome
                    .unchanged
                    .push((dotted.to_string(), source_canonical));
            } else {
                writes.push(make_write());
                outcome.written.push((dotted.to_string(), source_canonical));
            }
        }
    }
}

/// A stable textual fingerprint of a value, for the ledger.
///
/// Object keys are emitted in sorted order so the same logical value always
/// produces the same string regardless of how either document happened to
/// order it.
#[must_use]
pub fn canonical_json(value: &Json) -> String {
    serde_json::to_string(&sorted(value)).unwrap_or_else(|_| String::from("null"))
}

fn sorted(value: &Json) -> Json {
    match value {
        Json::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = JsonMap::new();
            for key in keys {
                out.insert(key.clone(), sorted(&map[key]));
            }
            Json::Object(out)
        }
        Json::Array(items) => Json::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn toml_item_to_json(item: &Item) -> Json {
    match item {
        Item::None => Json::Null,
        Item::Value(value) => toml_value_to_json(value),
        Item::Table(table) => toml_table_to_json(table),
        Item::ArrayOfTables(tables) => Json::Array(tables.iter().map(toml_table_to_json).collect()),
    }
}

fn toml_table_to_json(table: &Table) -> Json {
    let mut map = JsonMap::new();
    for (key, item) in table.iter() {
        map.insert(key.to_string(), toml_item_to_json(item));
    }
    Json::Object(map)
}

fn toml_value_to_json(value: &TomlValue) -> Json {
    match value {
        TomlValue::String(s) => Json::String(s.value().clone()),
        TomlValue::Integer(i) => Json::Number((*i.value()).into()),
        TomlValue::Float(f) => {
            serde_json::Number::from_f64(*f.value()).map_or(Json::Null, Json::Number)
        }
        TomlValue::Boolean(b) => Json::Bool(*b.value()),
        TomlValue::Datetime(d) => Json::String(d.value().to_string()),
        TomlValue::Array(array) => Json::Array(array.iter().map(toml_value_to_json).collect()),
        TomlValue::InlineTable(inline) => {
            let mut map = JsonMap::new();
            for (key, nested) in inline.iter() {
                map.insert(key.to_string(), toml_value_to_json(nested));
            }
            Json::Object(map)
        }
    }
}
