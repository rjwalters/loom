//! The one shared `owner/repo` slug validator (#13567).
//!
//! A slug is handed to `gh` and to API paths, so it must not be able to smuggle
//! a flag, a path or a host. Previously four hand-rolled validators answered
//! this with four different rules; this is the single canonical one.

/// Whether `slug` is a valid `owner/repo` slug: exactly one `/`; each segment
/// 1–100 bytes of `[A-Za-z0-9._-]`; the owner starts with neither `-` nor `.`;
/// the repo does not start with `-` and is not `.` or `..` (a leading `.` is
/// allowed on the repo half so `owner/.github` is accepted).
#[must_use]
pub fn valid_slug(slug: &str) -> bool {
    let mut parts = slug.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let chars_ok = |seg: &str| {
        (1..=100).contains(&seg.len())
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    chars_ok(owner)
        && chars_ok(repo)
        && !owner.starts_with(['-', '.'])
        && !repo.starts_with('-')
        && repo != "."
        && repo != ".."
}

#[cfg(test)]
mod tests {
    use super::valid_slug;

    #[test]
    fn slug_rule_is_table_driven() {
        let long = "a".repeat(101);
        let max = "a".repeat(100);
        for ok in [
            "o/r",
            "rjwalters/loom",
            "2AMLogic/2am",
            "a-b/c.d_e",
            "a.b-c_d/e.f",
            "acme/private",
            "owner/kicad-tools.git",
            "owner/.github",
            &format!("{max}/{max}"),
        ] {
            assert!(valid_slug(ok), "{ok:?} should be accepted");
        }
        for bad in [
            "",
            "o",
            "acme",
            "o/",
            "/r",
            "/",
            "a/b/c",
            "-a/b",
            ".a/b",
            "a/-b",
            "a/.",
            "a/..",
            "..",
            "./x",
            "a b/c",
            "a/lo om",
            "a/b;ls",
            "o/r?x",
            "github.com/o/r/x",
            "https://github.com/o/r",
            &format!("{long}/r"),
            &format!("o/{long}"),
        ] {
            assert!(!valid_slug(bad), "{bad:?} should be rejected");
        }
    }
}
