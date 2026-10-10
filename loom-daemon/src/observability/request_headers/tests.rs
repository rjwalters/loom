use super::*;
use tempfile::tempdir;

/// A synthetic value no error, log or `Debug` rendering may ever contain.
const SECRET: &str = "fixture-secret-7f3a9c";

/// Write `contents` to a fresh file with `mode` and return its directory
/// guard and path.
fn headers_file(contents: &str, mode: u32) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("otlp-headers");
    std::fs::write(&path, contents).unwrap();
    set_mode(&path, mode);
    (dir, path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

fn pairs(headers: &RequestHeaders) -> Vec<(String, String)> {
    headers
        .entries
        .iter()
        .map(|(name, value)| (name.as_str().to_string(), value.to_str().unwrap().to_string()))
        .collect()
}

fn line_error(text: &str) -> (usize, LineError) {
    match RequestHeaders::parse(text) {
        Err(HeadersFileProblem::Line { line, error }) => (line, error),
        other => panic!("expected a line error, got {other:?}"),
    }
}

#[test]
fn parses_name_value_lines_skipping_blanks_and_comment_lines() {
    let text = "# proxy credentials\n\
                \n\
                X-Client-Id: fixture-id\r\n\
                \t  \n\
                  # an indented comment\n\
                X-Client-Secret:   spaced out value \t\n\
                X-No-Space:tight";
    let headers = RequestHeaders::parse(text).unwrap();
    assert_eq!(
        pairs(&headers),
        vec![
            ("x-client-id".to_string(), "fixture-id".to_string()),
            ("x-client-secret".to_string(), "spaced out value".to_string()),
            ("x-no-space".to_string(), "tight".to_string()),
        ]
    );
    assert_eq!(headers.len(), 3);
    assert!(!headers.is_empty());
    assert!(!headers.sets_authorization());
}

#[test]
fn everything_after_the_first_colon_is_the_value_including_hash_and_colon() {
    // No inline comments: `#` and `:` are legal value characters, and
    // truncating at either would silently corrupt a credential.
    let headers =
        RequestHeaders::parse("Authorization: Basic abc:def#ghi # not a comment").unwrap();
    assert_eq!(
        pairs(&headers),
        vec![("authorization".to_string(), "Basic abc:def#ghi # not a comment".to_string())]
    );
    assert!(headers.sets_authorization());
}

#[test]
fn authorization_is_recognized_case_insensitively() {
    assert!(RequestHeaders::parse("AUTHORIZATION: x")
        .unwrap()
        .sets_authorization());
}

#[test]
fn every_http_token_character_is_a_legal_name_character() {
    let headers = RequestHeaders::parse("a!$%&'*+-.^_`|~9Z: v").unwrap();
    assert_eq!(headers.names().collect::<Vec<_>>(), vec!["a!$%&'*+-.^_`|~9z"]);
}

#[test]
fn malformed_lines_are_refused_with_their_one_based_line_number() {
    for (text, expected) in [
        ("X-Ok: 1\nno colon here", (2, LineError::MissingColon)),
        ("# c\n\n: value", (3, LineError::InvalidName)),
        ("X Name: value", (1, LineError::InvalidName)),
        ("X-Name : value", (1, LineError::InvalidName)),
        ("X(Name): value", (1, LineError::InvalidName)),
        ("X-Näme: value", (1, LineError::InvalidName)),
        ("\"X-Name\": value", (1, LineError::InvalidName)),
        ("X-Name:", (1, LineError::EmptyValue)),
        ("X-Name:   \t", (1, LineError::EmptyValue)),
        ("X-Name: caf\u{e9}", (1, LineError::InvalidValue)),
        ("X-Name: a\u{0}b", (1, LineError::InvalidValue)),
        ("X-Name: a\rb", (1, LineError::InvalidValue)),
        ("X-A: 1\nX-B: 2\nx-a: 3", (3, LineError::DuplicateName)),
        ("Content-Type: text/plain", (1, LineError::ExporterOwned)),
        ("X-A: 1\nHOST: elsewhere", (2, LineError::ExporterOwned)),
        ("Content-Length: 3", (1, LineError::ExporterOwned)),
        ("Transfer-Encoding: chunked", (1, LineError::ExporterOwned)),
        ("Connection: close", (1, LineError::ExporterOwned)),
    ] {
        assert_eq!(line_error(text), expected, "{text:?}");
    }
}

#[test]
fn a_file_with_no_headers_is_refused() {
    for text in ["", "\n\n", "# only a comment\n"] {
        assert_eq!(RequestHeaders::parse(text).err(), Some(HeadersFileProblem::NoHeaders));
    }
}

/// The acceptance criterion: a refusal names the file and the line, and
/// nothing read from the file — whichever part of the line was wrong.
#[test]
fn refusals_name_the_file_and_line_but_never_the_contents() {
    for (contents, line) in [
        (format!("X-Ok: fine\n{SECRET}\n"), 2),
        (format!("X-Bad Name-{SECRET}: {SECRET}\n"), 1),
        (format!("X-Ok: {SECRET}\nX-Ok: {SECRET}\n"), 2),
        (format!("\n\nX-Value: {SECRET}\u{7}\n"), 3),
        (format!("Content-Type: {SECRET}\n"), 1),
        (format!("{SECRET}:\n"), 1),
    ] {
        let (_dir, path) = headers_file(&contents, 0o600);
        let error = RequestHeaders::load(&path).unwrap_err();
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains(SECRET), "leaked into: {rendered}");
        }
        let shown = error.to_string();
        assert!(shown.contains(&path.display().to_string()), "{shown}");
        assert!(shown.contains(&format!("line {line}:")), "{shown}");
    }
}

#[cfg(unix)]
#[test]
fn a_group_or_world_accessible_file_is_refused_before_it_is_parsed() {
    for mode in [0o640, 0o604, 0o644, 0o660, 0o620, 0o601, 0o777] {
        // Deliberately unparseable: a permissions refusal must come first,
        // so a loose file's contents are never even classified.
        let (_dir, path) = headers_file(&format!("{SECRET}\n"), mode);
        let error = RequestHeaders::load(&path).unwrap_err();
        assert_eq!(error.problem, HeadersFileProblem::LoosePermissions(mode), "{mode:o}");
        let shown = error.to_string();
        assert!(shown.contains(&path.display().to_string()), "{shown}");
        assert!(shown.contains(&format!("mode {mode:04o}")), "{shown}");
        assert!(!shown.contains(SECRET), "{shown}");
    }
}

#[cfg(unix)]
#[test]
fn an_owner_only_file_is_accepted() {
    for mode in [0o600, 0o400] {
        let (_dir, path) = headers_file(&format!("X-Client-Secret: {SECRET}\n"), mode);
        let headers = RequestHeaders::load(&path).unwrap();
        assert_eq!(headers.names().collect::<Vec<_>>(), vec!["x-client-secret"], "{mode:o}");
    }
}

#[test]
fn a_missing_file_a_directory_and_an_oversized_file_are_refused_by_path() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("absent");
    let error = RequestHeaders::load(&missing).unwrap_err();
    assert!(matches!(error.problem, HeadersFileProblem::Unreadable(_)));
    assert!(error.to_string().contains(&missing.display().to_string()));

    let error = RequestHeaders::load(dir.path()).unwrap_err();
    assert_eq!(error.problem, HeadersFileProblem::NotRegularFile);

    let padding = "#".repeat(usize::try_from(MAX_HEADERS_FILE_BYTES).unwrap());
    let (_dir, big) = headers_file(&format!("X-A: {SECRET}\n{padding}\n"), 0o600);
    let error = RequestHeaders::load(&big).unwrap_err();
    assert_eq!(error.problem, HeadersFileProblem::TooLarge);
    assert!(!error.to_string().contains(SECRET));

    let (_dir, binary) = headers_file("", 0o600);
    std::fs::write(&binary, [b'X', b':', b' ', 0xff, 0xfe]).unwrap();
    assert_eq!(RequestHeaders::load(&binary).unwrap_err().problem, HeadersFileProblem::NotUtf8);
}

#[test]
fn debug_output_shows_names_and_redacts_every_value() {
    let headers =
        RequestHeaders::parse(&format!("X-Client-Id: {SECRET}-id\nAuthorization: Bearer {SECRET}"))
            .unwrap();
    for rendered in [format!("{headers:?}"), format!("{headers:#?}")] {
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(rendered.contains("x-client-id"), "{rendered}");
        assert!(rendered.contains("authorization"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
    // The value handed to the HTTP stack is marked sensitive too, so the
    // client's own `Debug` of a request does not print it either.
    for (_, value) in &headers.entries {
        assert!(value.is_sensitive());
        assert!(!format!("{value:?}").contains(SECRET));
    }
}

#[test]
fn the_file_is_read_again_on_every_load() {
    let (_dir, path) = headers_file("X-Token: first\n", 0o600);
    assert_eq!(pairs(&RequestHeaders::load(&path).unwrap())[0].1, "first");
    std::fs::write(&path, "X-Token: rotated\n").unwrap();
    assert_eq!(pairs(&RequestHeaders::load(&path).unwrap())[0].1, "rotated");
}

#[test]
fn entry_policy_is_a_no_op_without_a_headers_file() {
    // Existing configs: any kind, any endpoint the other checks allow.
    assert_eq!(entry_policy(false, "http://collector.internal:4318", None), Ok(()));
    assert_eq!(entry_policy(true, "http://collector.internal:4318", None), Ok(()));
}

#[test]
fn entry_policy_requires_otlp_a_path_and_an_encrypted_or_loopback_endpoint() {
    let path = Some("/run/secrets/otlp-headers");
    for endpoint in [
        "https://otlp.internal",
        "https://otlp.internal:4318/prefix",
        "http://127.0.0.1:4318",
        "http://localhost:4318",
        "http://[::1]:4318",
    ] {
        assert_eq!(entry_policy(true, endpoint, path), Ok(()), "{endpoint}");
    }
    for endpoint in ["http://collector.internal:4318", "http://10.0.0.5:4318"] {
        let detail = entry_policy(true, endpoint, path).unwrap_err();
        assert!(detail.contains("cleartext"), "{detail}");
    }
    assert!(entry_policy(false, "https://otlp.internal", path)
        .unwrap_err()
        .contains("only supported on an otlp"));
    for empty in ["", "   "] {
        assert!(entry_policy(true, "https://otlp.internal", Some(empty))
            .unwrap_err()
            .contains("non-empty"));
    }
}
