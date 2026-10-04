//! Inbox mail driven from the notice stream (#10169).

use std::cell::RefCell;

use super::fake::{issue, pr, repo_input, t, Host, World, STAR};
use crate::star_liveness::mail::{dispatch_notices, mail_key, MailSink, NoopMailSink};

#[derive(Default)]
struct Recorder {
    sent: RefCell<Vec<String>>,
    resolved: RefCell<Vec<String>>,
}

impl MailSink for Recorder {
    fn send(&self, key: &str, _title: &str, _body: &str) {
        self.sent.borrow_mut().push(key.to_string());
    }
    fn resolve(&self, key: &str) {
        self.resolved.borrow_mut().push(key.to_string());
    }
}

/// Seed the watchdog-stall shape (`NoProgress`, a mailing kind) and return
/// the repo inputs. The ask appears on a pass at 10:31.
fn seed_stall(world: &World, slug: &str) {
    world.add(slug, issue(7, &[STAR, "loom:building"]));
    world.add(slug, pr(8, 7, &["loom:review-requested"]));
}

/// Dispatch what `h` has accumulated since the last drain.
fn drain(h: &mut Host, sink: &dyn MailSink) {
    dispatch_notices(sink, &h.notices);
    h.notices.clear();
}

#[test]
fn send_once_then_resolve_on_clear_with_the_same_key() {
    let world = World::default();
    let slug = "n/mail";
    seed_stall(&world, slug);
    let repos = vec![repo_input(slug)];
    let mut h = Host::new("host-a");
    let rec = Recorder::default();

    h.pass(&world, &repos, Vec::new(), t(10, 0));
    h.pass(&world, &repos, Vec::new(), t(10, 31));
    drain(&mut h, &rec);
    assert_eq!(rec.sent.borrow().len(), 1, "one mail for the new ask");

    h.pass(&world, &repos, Vec::new(), t(10, 33));
    drain(&mut h, &rec);
    assert_eq!(rec.sent.borrow().len(), 1, "no repeat on later passes");

    world.repo(slug).items.get_mut(&8).unwrap().labels = vec!["loom:pr".into()];
    h.pass(&world, &repos, Vec::new(), t(10, 35));
    drain(&mut h, &rec);
    assert_eq!(rec.sent.borrow().len(), 1);
    assert_eq!(*rec.resolved.borrow(), *rec.sent.borrow(), "same key");
}

#[test]
fn merge_risk_hold_does_not_mail() {
    let world = World::default();
    let slug = "n/held";
    world.add(slug, issue(1, &[STAR, "loom:building"]));
    world.add(slug, pr(2, 1, &["loom:pr", "loom:operator"]));
    let mut h = Host::new("host-a");
    h.pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert_eq!(h.notices.len(), 1);
    let rec = Recorder::default();
    dispatch_notices(&rec, &h.notices);
    assert!(rec.sent.borrow().is_empty());
}

#[test]
fn unconfigured_sink_is_a_noop() {
    let world = World::default();
    let slug = "n/none";
    seed_stall(&world, slug);
    let mut h = Host::new("host-a");
    let repos = vec![repo_input(slug)];
    h.pass(&world, &repos, Vec::new(), t(10, 0));
    h.pass(&world, &repos, Vec::new(), t(10, 31));
    assert_eq!(h.notices.len(), 1);
    dispatch_notices(&NoopMailSink, &h.notices);
}

#[test]
fn mail_key_is_sanitized_and_capped() {
    let world = World::default();
    let slug = "n/key";
    seed_stall(&world, slug);
    let mut h = Host::new("host-a");
    let repos = vec![repo_input(slug)];
    h.pass(&world, &repos, Vec::new(), t(10, 0));
    h.pass(&world, &repos, Vec::new(), t(10, 31));
    let key = mail_key(&h.notices[0]);
    assert!(key.starts_with("mail-n-key-starliveness-7-"));
    assert!(key.len() <= 200);
    assert!(key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)));
}
