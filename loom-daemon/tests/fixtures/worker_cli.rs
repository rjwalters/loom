//! Native fake harness: tests argv, streams, exit status and exec identity.
fn main() {
    if std::env::var("FIXTURE_SIGNAL").is_ok() {
        std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
    }
    println!("pid={}", std::process::id());
    for arg in std::env::args_os().skip(1) {
        println!("arg={arg:?}");
    }
    for key in [
        "LOOM_RUNTIME",
        "LOOM_MODEL",
        "LOOM_TEST_ALLOW_SYSTEMD",
        "LOOM_DAEMON_LOG",
    ] {
        println!("{key}={}", std::env::var(key).unwrap_or_default());
    }
    if let Ok(source) = std::env::var("LOOM_TEST_PROVIDER_SECRET") {
        println!(
            "credential_alias_matches={}",
            std::env::var("LOOM_TEST_HARNESS_SECRET").ok().as_deref() == Some(source.as_str())
        );
    }
    eprintln!("fixture stderr");
    std::process::exit(
        std::env::var("FIXTURE_EXIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    );
}
