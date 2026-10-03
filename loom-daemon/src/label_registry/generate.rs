//! Generates the Loom marker block of `labels.yml` from the registry.
//!
//! The output is byte-for-byte what both full `labels.yml` copies contain
//! (the file is nothing but the marker block), and exactly what
//! `init::scaffolding` merges into an installed repo.

use super::Registry;

/// Block markers; must equal `init::scaffolding::LOOM_LABELS_START/END`
/// (the installer merges exactly this range).
pub const LOOM_LABELS_START: &str = "# BEGIN LOOM LABELS";
pub const LOOM_LABELS_END: &str = "# END LOOM LABELS";

fn yaml_dq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The full `# BEGIN LOOM LABELS` … `# END LOOM LABELS` block, newline-terminated.
#[must_use]
pub fn loom_block(reg: &Registry) -> String {
    let mut out: Vec<String> = vec![LOOM_LABELS_START.to_string()];
    out.extend(reg.yaml_header.iter().cloned());
    for (i, l) in reg.labels.iter().enumerate() {
        if i > 0 {
            out.push(String::new());
        }
        out.extend(l.yaml_preamble.iter().cloned());
        out.push(format!("- name: {}", l.name));
        out.push(format!("  description: {}", yaml_dq(&l.description)));
        match &l.color_note {
            Some(n) => out.push(format!("  color: \"{}\"  # {n}", l.color)),
            None => out.push(format!("  color: \"{}\"", l.color)),
        }
    }
    out.push(LOOM_LABELS_END.to_string());
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Names declared in the Loom block of a `labels.yml` (entries only).
#[must_use]
pub fn block_names(yaml: &str) -> Vec<String> {
    yaml.lines()
        .filter_map(|l| l.strip_prefix("- name: "))
        .map(|n| n.trim().to_string())
        .collect()
}
