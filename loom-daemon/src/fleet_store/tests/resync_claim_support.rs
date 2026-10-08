//! An in-memory forge for the resync claim: the git refs and commits API
//! behind the [`Transport`] / [`WriteTransport`] seams. No network.
//!
//! It answers the way GitHub was observed to on 2026-10-08 (see the module
//! docs of `resync_claim`): creating a ref succeeds once and is `422` after
//! that, deleting a missing ref is `422`, and a `PATCH` outside `refs/heads`
//! is accepted whether or not it is a fast-forward.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::fleet_store::fetch::{Reply, Transport};
use crate::fleet_store::propose::WriteTransport;

/// What a scripted fault does to the one call it matches.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Fault {
    /// Answer with this HTTP status and do nothing.
    Status(u16),
    /// Fail the request without doing anything.
    Unreachable,
    /// Do it, then lose the reply.
    LostReply,
}

/// A called-before-every-request hook, for interleaving a second host.
type Hook = Box<dyn Fn(&FakeRefForge, &str, &str)>;

/// The fake forge. Shared by reference between "hosts" in one test.
#[derive(Default)]
pub(crate) struct FakeRefForge {
    /// Full ref name to sha.
    pub(crate) refs: RefCell<BTreeMap<String, String>>,
    /// Commit sha to (message, parents).
    pub(crate) commits: RefCell<BTreeMap<String, (String, Vec<String>)>>,
    /// Every request, as `"METHOD path"`.
    pub(crate) calls: RefCell<Vec<String>>,
    faults: RefCell<Vec<(String, Fault)>>,
    hook: RefCell<Option<Hook>>,
    next: Cell<u64>,
}

impl FakeRefForge {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fault the next request whose `"METHOD path"` contains `needle`.
    pub(crate) fn fault(&self, needle: &str, fault: Fault) {
        self.faults.borrow_mut().push((needle.to_string(), fault));
    }

    /// Run `hook` before every request (it sees the method and path).
    pub(crate) fn before(&self, hook: impl Fn(&FakeRefForge, &str, &str) + 'static) {
        *self.hook.borrow_mut() = Some(Box::new(hook));
    }

    /// Record a commit directly and return its sha.
    pub(crate) fn add_commit(&self, message: &str, parents: &[&str]) -> String {
        let n = self.next.get() + 1;
        self.next.set(n);
        let sha = format!("{n:040x}");
        self.commits.borrow_mut().insert(
            sha.clone(),
            (message.to_string(), parents.iter().map(|p| (*p).to_string()).collect()),
        );
        sha
    }

    /// Point `name` at `sha` directly.
    pub(crate) fn set_ref(&self, name: &str, sha: &str) {
        self.refs
            .borrow_mut()
            .insert(name.to_string(), sha.to_string());
    }

    pub(crate) fn ref_sha(&self, name: &str) -> Option<String> {
        self.refs.borrow().get(name).cloned()
    }

    /// Requests that change something (everything but a `GET`).
    pub(crate) fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|c| !c.starts_with("GET "))
            .cloned()
            .collect()
    }

    fn enter(&self, method: &str, path: &str) -> Option<Fault> {
        // Take the hook out while it runs, so it may call back in.
        let hook = self.hook.borrow_mut().take();
        if let Some(h) = hook {
            h(self, method, path);
            let mut slot = self.hook.borrow_mut();
            if slot.is_none() {
                *slot = Some(h);
            }
        }
        let call = format!("{method} {path}");
        self.calls.borrow_mut().push(call.clone());
        let mut faults = self.faults.borrow_mut();
        let at = faults
            .iter()
            .position(|(needle, _)| call.contains(needle))?;
        Some(faults.remove(at).1)
    }

    fn handle(&self, method: &str, path: &str, body: &Value) -> Reply {
        let rest = path.split_once("/git/").map_or("", |(_, r)| r);
        let reply = |status: u16, body: Value| Reply {
            status,
            etag: None,
            body: if body.is_null() {
                String::new()
            } else {
                body.to_string()
            },
        };
        let missing = || reply(422, json!({"message": "Reference does not exist"}));
        let field = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match (method, rest) {
            ("POST", "commits") => {
                let parents: Vec<String> = body
                    .get("parents")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| p.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
                let sha = self.add_commit(&field("message"), &parents);
                reply(201, json!({"sha": sha}))
            }
            ("POST", "refs") => {
                let name = field("ref");
                let mut refs = self.refs.borrow_mut();
                if refs.contains_key(&name) {
                    return reply(422, json!({"message": "Reference already exists"}));
                }
                refs.insert(name.clone(), field("sha"));
                reply(201, json!({"ref": name}))
            }
            ("PATCH", name) if name.starts_with("refs/") => {
                let mut refs = self.refs.borrow_mut();
                let Some(slot) = refs.get_mut(name) else {
                    return missing();
                };
                *slot = field("sha");
                reply(200, json!({"ref": name}))
            }
            ("DELETE", name) if name.starts_with("refs/") => {
                if self.refs.borrow_mut().remove(name).is_some() {
                    reply(204, Value::Null)
                } else {
                    missing()
                }
            }
            ("GET", name) if name.starts_with("ref/") => {
                let full = format!("refs/{}", &name["ref/".len()..]);
                match self.refs.borrow().get(&full) {
                    Some(sha) => reply(200, json!({"ref": full, "object": {"sha": sha}})),
                    None => reply(404, json!({"message": "Not Found"})),
                }
            }
            ("GET", name) if name.starts_with("matching-refs/") => {
                let prefix = format!("refs/{}", &name["matching-refs/".len()..]);
                let found: Vec<Value> = self
                    .refs
                    .borrow()
                    .iter()
                    .filter(|(full, _)| full.starts_with(&prefix))
                    .map(|(full, sha)| json!({"ref": full, "object": {"sha": sha}}))
                    .collect();
                reply(200, Value::Array(found))
            }
            ("GET", name) if name.starts_with("commits/") => {
                match self.commits.borrow().get(&name["commits/".len()..]) {
                    Some((message, _)) => reply(200, json!({"message": message})),
                    None => reply(404, json!({"message": "Not Found"})),
                }
            }
            _ => reply(400, json!({"message": format!("fake forge: unhandled {method} {path}")})),
        }
    }

    fn request(&self, method: &str, path: &str, body: &Value) -> Result<Reply> {
        match self.enter(method, path) {
            Some(Fault::Status(status)) => Ok(Reply {
                status,
                etag: None,
                body: json!({"message": "scripted"}).to_string(),
            }),
            Some(Fault::Unreachable) => bail!("scripted: {method} {path} could not be made"),
            Some(Fault::LostReply) => {
                self.handle(method, path, body);
                bail!("scripted: {method} {path} timed out (a write may still have been applied)")
            }
            None => Ok(self.handle(method, path, body)),
        }
    }
}

/// A handle on a shared [`FakeRefForge`], for code that wants to own its
/// forge (the workspace pass builds one per repo).
pub(crate) struct Shared(pub(crate) std::rc::Rc<FakeRefForge>);

impl Transport for Shared {
    fn get(&self, api_path: &str, a: Option<&str>, e: Option<&str>) -> Result<Reply> {
        self.0.get(api_path, a, e)
    }
}

impl WriteTransport for Shared {
    fn write(&self, method: &str, api_path: &str, body: &Value) -> Result<Reply> {
        self.0.write(method, api_path, body)
    }
}

impl Transport for FakeRefForge {
    fn get(&self, api_path: &str, _: Option<&str>, _: Option<&str>) -> Result<Reply> {
        self.request("GET", api_path, &Value::Null)
    }
}

impl WriteTransport for FakeRefForge {
    fn write(&self, method: &str, api_path: &str, body: &Value) -> Result<Reply> {
        self.request(method, api_path, body)
    }
}
