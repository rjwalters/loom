//! The SigNoz read client: URL binding, the request shape against a loopback
//! listener (no credential header), and endpoint resolution.

use super::*;

fn reader(endpoint: &str) -> TelemetryReader {
    TelemetryReader {
        endpoint: endpoint.to_string(),
        timeout: std::time::Duration::from_secs(10),
    }
}

#[test]
fn every_parameter_is_bound_as_a_url_parameter() {
    let http = reader("https://reader.example:14341");
    let params = [("repo", "acme/app"), ("host", "h1"), ("limit", "7")]
        .map(|(n, v)| (n.to_string(), v.to_string()));
    let url = http.url_with(&params).unwrap();
    assert_eq!(url.path(), QUERY_PATH);
    let pairs: std::collections::BTreeMap<String, String> =
        url.query_pairs().into_owned().collect();
    assert_eq!(pairs["param_repo"], "acme/app");
    assert_eq!(pairs["param_host"], "h1");
    assert_eq!(pairs["param_limit"], "7");
    assert_eq!(pairs.len(), 3, "{pairs:?}");
    // Only `param_<name>` keys: the reader refuses any other.
    assert!(pairs.keys().all(|k| k.starts_with("param_")), "{pairs:?}");

    let arbitrary = http
        .url_with(&[("as_of_ns".to_string(), "5".to_string())])
        .unwrap();
    assert_eq!(arbitrary.as_str(), "https://reader.example:14341/v1/query?param_as_of_ns=5");
    // A trailing slash is still the bare base; no parameters, no `?`.
    let bare = reader("http://reader.example:14341/")
        .url_with(&[])
        .unwrap();
    assert_eq!(bare.as_str(), "http://reader.example:14341/v1/query");
}

#[test]
fn an_endpoint_that_is_not_a_bare_http_base_is_refused_and_never_echoed() {
    for endpoint in [
        "file:///etc/passwd",
        "not a url",
        "http://reader.example:14341/v1/query",
        "http://reader.example:14341/?user=x",
        "http://reader.example:14341/#frag",
        "http://u:s3cret@reader.example:14341",
    ] {
        let Err(ReadError::Unavailable(why)) = reader(endpoint).url_with(&[]) else {
            panic!("{endpoint} must be refused");
        };
        assert!(why.starts_with("invalid reader endpoint"), "{why}");
        assert!(!why.contains("s3cret"), "{why}");
    }
    assert!(reader("not a url").post_with("SELECT 1", &[]).is_err());
}

/// One request as a local listener received it.
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Serve one request on a loopback port with `status` and `reply`; the
/// listener's base URL and a handle yielding what it received.
fn one_shot(status: &'static str, reply: &'static str) -> (String, std::thread::JoinHandle<Seen>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (k, v) = line.split_once(':').unwrap();
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
        let len: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .map_or(0, |(_, v)| v.parse().unwrap());
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
        Seen {
            request_line: request_line.trim_end().to_string(),
            headers,
            body: String::from_utf8(body).unwrap(),
        }
    });
    (base, handle)
}

#[test]
fn a_query_posts_the_sql_as_the_body_and_params_in_the_url_with_no_credential() {
    let (base, server) = one_shot("200 OK", "{\"n\":1}\n");
    let sql = "SELECT count() AS n FROM t WHERE x = {v:String} FORMAT JSONEachRow";
    let got = reader(&base)
        .post_with(sql, &[("v".to_string(), "1; DROP TABLE x".to_string())])
        .unwrap();
    assert_eq!(got, "{\"n\":1}\n");
    let seen = server.join().unwrap();
    assert_eq!(seen.request_line, "POST /v1/query?param_v=1%3B+DROP+TABLE+x HTTP/1.1");
    assert_eq!(seen.body, sql, "the statement is the body, unaltered");
    for (name, _) in &seen.headers {
        assert!(
            !name.starts_with("x-clickhouse") && name != "authorization",
            "no credential header may be sent: {:?}",
            seen.headers
        );
    }
}

#[test]
fn a_reader_error_status_is_refused_with_its_answer() {
    let (base, server) = one_shot("403 Forbidden", "unknown_caller");
    let Err(ReadError::Refused(why)) = reader(&base).post_with("SELECT 1", &[]) else {
        panic!("a 403 must be Refused");
    };
    assert!(why.contains("403") && why.contains("unknown_caller"), "{why}");
    server.join().unwrap();
}

#[test]
fn endpoint_config_reads_the_env_then_the_config_and_names_retired_keys() {
    let config = serde_json::json!({
        "telemetry": {"signoz": {
            "endpoint": " https://reader.example:14341 ",
            "user": "reader",
            "keyFile": "/home/me/.config/loom/signoz.key"
        }},
        "autonomous": {"eta": {"fleetRefresh": {"signoz": {
            "endpoint": "https://old.example:8443"
        }}}}
    });
    let c = EndpointConfig::resolve(&config, None);
    assert_eq!(c.endpoint.as_deref(), Some("https://reader.example:14341"));
    assert_eq!(
        c.ignored,
        [
            "telemetry.signoz.user",
            "telemetry.signoz.keyFile",
            "autonomous.eta.fleetRefresh.signoz"
        ]
    );
    // The environment wins; a blank one is unset.
    let env = EndpointConfig::resolve(&config, Some("http://env.example:14341"));
    assert_eq!(env.endpoint.as_deref(), Some("http://env.example:14341"));
    let blank = EndpointConfig::resolve(&config, Some("  "));
    assert_eq!(blank.endpoint.as_deref(), Some("https://reader.example:14341"));

    // The retired ETA block is never read as an endpoint.
    let retired_only = EndpointConfig::resolve(
        &serde_json::json!({"autonomous": {"eta": {"fleetRefresh": {"signoz": {
            "endpoint": "https://old.example:8443"}}}}}),
        None,
    );
    assert_eq!(retired_only.endpoint, None);
    assert_eq!(EndpointConfig::resolve(&serde_json::json!({}), None), EndpointConfig::default());
}
