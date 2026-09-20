use super::{field, truncate, Request};
use anyhow::{bail, Context, Result};
use std::{fs, path::Path};

pub(super) fn execute(cwd: &Path, request: &Request) -> Result<String> {
    let path = cwd.join(field(&request.input, "path")?);
    match request.tool.as_str() {
        "read" => {
            if fs::metadata(&path)?.len() > 8 * 1024 * 1024 {
                bail!("file exceeds 8 MiB; use a bounded shell read");
            }
            let data = fs::read_to_string(&path).context("cannot read UTF-8 file")?;
            let offset = request
                .input
                .get("offset")
                .and_then(|v| v.as_u64())
                .unwrap_or(1)
                .saturating_sub(1) as usize;
            let limit = request
                .input
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(200)
                .min(2000) as usize;
            Ok(truncate(
                data.lines()
                    .enumerate()
                    .skip(offset)
                    .take(limit)
                    .map(|(i, line)| format!("{}: {line}\n", i + 1))
                    .collect(),
            ))
        }
        "write" => {
            let text = field(&request.input, "content")?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, text)?;
            Ok(format!("Wrote {} bytes to {}", text.len(), path.display()))
        }
        "edit" => {
            let old = field(&request.input, "oldText")?;
            let new = field(&request.input, "newText")?;
            let original = fs::read_to_string(&path)?;
            if old.is_empty() || original.matches(old).count() != 1 {
                bail!(
                    "edit requires exactly one nonempty oldText match; read the current file first"
                );
            }
            fs::write(&path, original.replacen(old, new, 1))?;
            Ok(format!("Edited {}", path.display()))
        }
        _ => bail!("unsupported file operation"),
    }
}
