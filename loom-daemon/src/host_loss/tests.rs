#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

use super::*;

/// An IMDS that answers from a table and counts every request.
#[derive(Default)]
struct Fake {
    docs: Vec<(&'static str, &'static str)>,
    calls: AtomicUsize,
}

impl Imds for Fake {
    fn get(&self, path: &str) -> Result<Option<String>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .docs
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, d)| (*d).to_string()))
    }
}

#[derive(Default)]
struct Mock {
    sweeps: Vec<InFlight>,
    closed: Mutex<Vec<(String, &'static str)>>,
    released: AtomicUsize,
    spans: Mutex<Vec<SpanRecord>>,
}

impl Effects for Mock {
    fn in_flight(&self) -> Vec<InFlight> {
        self.sweeps.clone()
    }
    fn close_span(&self, sweep: &InFlight, notice: &Notice) {
        self.closed
            .lock()
            .unwrap()
            .push((sweep.sweep_id.clone(), notice.kind.as_str()));
    }
    fn release_assignments(&self) -> usize {
        self.released.fetch_add(1, Ordering::SeqCst);
        2
    }
    fn emit(&self, span: SpanRecord) {
        self.spans.lock().unwrap().push(span);
    }
    fn instance_type(&self) -> Option<String> {
        Some("c7g.4xlarge".into())
    }
}

fn sweep(id: &str) -> InFlight {
    InFlight {
        root: "/w".into(),
        sweep_id: id.into(),
    }
}

const SPOT: &str = r#"{"action":"terminate","time":"2026-10-20T08:22:00Z"}"#;
const REBALANCE: &str = r#"{"noticeTime":"2026-10-20T08:10:00Z"}"#;

#[test]
fn a_spot_notice_is_handled_once_and_ends_the_in_flight_sweeps() {
    let imds = Fake {
        docs: vec![(imds::SPOT_ACTION_PATH, SPOT)],
        ..Fake::default()
    };
    let fx = Mock {
        sweeps: vec![sweep("step-builder-1-a"), sweep("sweep-2")],
        ..Mock::default()
    };
    let mut seen = HashSet::new();
    let first = poll(&imds, &mut seen, Utc::now());
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].kind, Kind::SpotReclaim);
    assert_eq!(first[0].action_time.unwrap().to_rfc3339(), "2026-10-20T08:22:00+00:00");
    assert!(poll(&imds, &mut seen, Utc::now()).is_empty(), "once per kind");

    handle(&first[0], &fx, "host-x");
    assert_eq!(
        *fx.closed.lock().unwrap(),
        vec![
            ("step-builder-1-a".to_string(), "spot_reclaim"),
            ("sweep-2".to_string(), "spot_reclaim")
        ]
    );
    assert_eq!(fx.released.load(Ordering::SeqCst), 1, "one release pass");
    let spans = fx.spans.lock().unwrap();
    assert_eq!(spans.len(), 1);
    let a = &spans[0].attributes;
    assert_eq!(spans[0].name.as_str(), "loom.host.interruption_notice");
    assert_eq!(a[HOST_ID], "host-x");
    assert_eq!(a[LOSS_CAUSE], "spot_reclaim");
    assert_eq!(a[INSTANCE_TYPE], "c7g.4xlarge");
    assert_eq!(a[IN_FLIGHT], "step-builder-1-a,sweep-2");
    assert_eq!(a[ASSIGNMENTS_RELEASED], "2");
    assert!(a.contains_key(NOTICE_TIME) && a.contains_key(ACTION_TIME));
    let exported = spans[0].clone().bounded().attributes;
    for key in a.keys() {
        assert!(exported.contains_key(key), "{key} is dropped at export");
    }
}

#[test]
fn a_rebalance_notice_maps_to_its_own_cause() {
    let imds = Fake {
        docs: vec![(imds::REBALANCE_PATH, REBALANCE)],
        ..Fake::default()
    };
    let mut seen = HashSet::new();
    let got = poll(&imds, &mut seen, Utc::now());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind, Kind::Rebalance);
    assert_eq!(got[0].notice_time.to_rfc3339(), "2026-10-20T08:10:00+00:00");
    assert_eq!(got[0].action_time, None);
    let fx = Mock {
        sweeps: vec![sweep("s")],
        ..Mock::default()
    };
    handle(&got[0], &fx, "h");
    assert_eq!(fx.closed.lock().unwrap()[0].1, "rebalance");
}

#[test]
fn a_404_on_both_endpoints_emits_nothing() {
    let imds = Fake::default();
    let mut seen = HashSet::new();
    assert!(poll(&imds, &mut seen, Utc::now()).is_empty());
    assert_eq!(imds.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn the_watcher_does_not_poll_while_idle() {
    let imds = Fake {
        docs: vec![(imds::SPOT_ACTION_PATH, SPOT)],
        ..Fake::default()
    };
    let fx = Mock::default();
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| run(&imds, &fx, "h", Duration::from_millis(5), &stop));
        std::thread::sleep(Duration::from_millis(60));
        stop.store(true, Ordering::Relaxed);
    });
    assert_eq!(imds.calls.load(Ordering::SeqCst), 0, "no work in flight: no IMDS traffic");
    assert!(fx.spans.lock().unwrap().is_empty());
}

#[test]
fn the_watcher_handles_a_notice_while_work_is_in_flight() {
    let imds = Fake {
        docs: vec![(imds::SPOT_ACTION_PATH, SPOT)],
        ..Fake::default()
    };
    let fx = Mock {
        sweeps: vec![sweep("s")],
        ..Mock::default()
    };
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| run(&imds, &fx, "h", Duration::from_millis(5), &stop));
        std::thread::sleep(Duration::from_millis(80));
        stop.store(true, Ordering::Relaxed);
    });
    assert_eq!(fx.spans.lock().unwrap().len(), 1, "exactly one notice span");
    assert_eq!(fx.closed.lock().unwrap().len(), 1);
}

/// The real client against a local server speaking IMDSv2: a token `PUT`,
/// then a `GET` carrying it; a 404 reads as no notice.
#[test]
fn the_http_client_speaks_imdsv2() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (mut c, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = c.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let reply = if req.starts_with("PUT /latest/api/token") {
                "HTTP/1.0 200 OK\r\n\r\nTOK"
            } else if req.contains("X-aws-ec2-metadata-token: TOK")
                && req.contains("spot/instance-action")
            {
                "HTTP/1.0 200 OK\r\n\r\n{\"action\":\"terminate\"}"
            } else {
                "HTTP/1.0 404 Not Found\r\n\r\n"
            };
            c.write_all(reply.as_bytes()).unwrap();
            seen.push(req);
        }
        seen
    });
    let http = imds::Http::new(&addr);
    assert_eq!(
        http.get(imds::SPOT_ACTION_PATH).unwrap().as_deref(),
        Some("{\"action\":\"terminate\"}")
    );
    assert_eq!(http.get(imds::REBALANCE_PATH).unwrap(), None);
    let seen = server.join().unwrap();
    assert!(seen[0].contains("X-aws-ec2-metadata-token-ttl-seconds"));
}

#[test]
fn a_non_ec2_host_never_builds_a_client() {
    // `spawn` consults DMI before it constructs anything; on a host whose DMI
    // is not Amazon it returns false and starts no thread.
    if !ec2::is_ec2() {
        assert!(!spawn(Vec::new));
    }
}
