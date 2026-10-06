//! `loom-qual-fixture` — the packaged artifact of the qualification fixture.
//!
//! `loom-qual-fixture newest <tag>...` prints the newest parseable version.
//! The release/artifact install smoke test runs the downloaded binary with a
//! known tag set and compares its output, so a corrupt or wrong-architecture
//! download fails visibly instead of being trusted on its checksum alone.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((cmd, rest)) if cmd == "newest" => {
            match loom_qual_fixture::newest(rest.iter().map(String::as_str)) {
                Some(v) => {
                    println!("{}.{}.{}", v.major, v.minor, v.patch);
                    ExitCode::SUCCESS
                }
                None => {
                    eprintln!("no parseable version");
                    ExitCode::from(1)
                }
            }
        }
        Some((cmd, _)) if cmd == "--version" => {
            println!("loom-qual-fixture {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: loom-qual-fixture newest <tag>... | --version");
            ExitCode::from(2)
        }
    }
}
