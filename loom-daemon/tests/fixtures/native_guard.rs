fn main() {
    match std::env::var("GUARD_FIXTURE_MODE").unwrap().as_str() {
        "invalid" => println!("not-json"),
        "unknown" => println!("{{\"unexpected\":true}}"),
        "crash" => std::process::exit(12),
        "timeout" => std::thread::sleep(std::time::Duration::from_secs(60)),
        _ => panic!("unknown fixture mode"),
    }
}
