//! Fake-`gh` building blocks for tests of the PR-side reconciliation passes
//! (#10349): REST pull rows, and the bash arm a fake `gh` script uses to serve
//! them for `gh api --include repos/…/pulls?state=open…`.

/// One REST `GET pulls` row (the fields [`crate::forge_pull_listing`] reads).
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) number: u32,
    pub(crate) labels: Vec<String>,
    pub(crate) head_ref: String,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) draft: bool,
    /// PR description (`None` = absent from the JSON).
    pub(crate) body: Option<String>,
    /// `user.login`.
    pub(crate) author: String,
    /// `author_association` (`None` = absent).
    pub(crate) association: Option<String>,
    /// `head.repo.full_name` (`None` = absent).
    pub(crate) head_repo: Option<String>,
}

/// A row for PR `number` carrying `labels`, on `feature/issue-<number>` at a
/// head of `number` in hex, otherwise defaulted.
pub(crate) fn row(number: u32, labels: &[&str]) -> Row {
    Row {
        number,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        head_ref: format!("feature/issue-{number}"),
        head_sha: format!("{number:040x}"),
        base_ref: "main".to_string(),
        created_at: "2026-10-01T00:00:00Z".to_string(),
        updated_at: "2026-10-01T00:00:00Z".to_string(),
        draft: false,
        body: None,
        author: "builder".to_string(),
        association: None,
        head_repo: None,
    }
}

impl Row {
    pub(crate) fn head(mut self, head_ref: &str) -> Self {
        head_ref.clone_into(&mut self.head_ref);
        self
    }
    pub(crate) fn sha(mut self, sha: &str) -> Self {
        sha.clone_into(&mut self.head_sha);
        self
    }
    pub(crate) fn updated(mut self, at: &str) -> Self {
        at.clone_into(&mut self.updated_at);
        self
    }
    pub(crate) fn created(mut self, at: &str) -> Self {
        at.clone_into(&mut self.created_at);
        self
    }
    /// The PR body (no `'` — the fixture echoes it inside single quotes).
    pub(crate) fn body(mut self, body: &str) -> Self {
        self.body = Some(body.to_string());
        self
    }
    /// The author login and association.
    pub(crate) fn author(mut self, login: &str, association: &str) -> Self {
        login.clone_into(&mut self.author);
        self.association = Some(association.to_string());
        self
    }
    /// The head repository's `owner/name`.
    pub(crate) fn repo(mut self, full_name: &str) -> Self {
        self.head_repo = Some(full_name.to_string());
        self
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        let labels: Vec<_> = self
            .labels
            .iter()
            .map(|l| serde_json::json!({ "name": l }))
            .collect();
        let mut v = serde_json::json!({
            "number": self.number,
            "state": "open",
            "draft": self.draft,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "user": { "login": self.author },
            "labels": labels,
            "head": { "ref": self.head_ref, "sha": self.head_sha },
            "base": { "ref": self.base_ref },
        });
        if let Some(body) = &self.body {
            v["body"] = serde_json::json!(body);
        }
        if let Some(association) = &self.association {
            v["author_association"] = serde_json::json!(association);
        }
        if let Some(repo) = &self.head_repo {
            v["head"]["repo"] = serde_json::json!({ "full_name": repo });
        }
        v
    }
}

/// A listing body (JSON array) for `rows`.
pub(crate) fn listing(rows: &[Row]) -> String {
    serde_json::Value::Array(rows.iter().map(Row::json).collect()).to_string()
}

/// A POSIX-`sh` arm answering the open-PR listing with `200` + the output of
/// the shell command `emit` (which must print the JSON body), then exiting.
/// Place it before any catch-all `api` arm.
pub(crate) fn pulls_arm_cmd(emit: &str) -> String {
    format!(
        "case \"$*\" in api*'pulls?state=open'*)\n  \
         printf 'HTTP/2.0 200 OK\\r\\n\\r\\n'\n  {emit}\n  exit 0 ;;\nesac\n"
    )
}

/// [`pulls_arm_cmd`] serving the fixed listing of `rows`.
pub(crate) fn pulls_arm(rows: &[Row]) -> String {
    pulls_arm_cmd(&format!("echo '{}'", listing(rows)))
}

/// A POSIX-`sh` arm answering every single-PR `GET pulls/<n>` (not its
/// `/files`, `/comments`, … sub-resources) with `200` and
/// `{"mergeable": <mergeable>}` (`true` / `false` / `null`) at the head
/// [`row`] gives PR `n` (`n` in 40-digit hex).
pub(crate) fn mergeable_arm(mergeable: &str) -> String {
    format!(
        "case \"$*\" in *'/pulls/'*'/'[a-z]*) ;; api*'/pulls/'[0-9]*)\n  \
         a=\"$*\"; n=\"${{a##*/pulls/}}\"; n=\"${{n%% *}}\"\n  \
         printf 'HTTP/2.0 200 OK\\r\\n\\r\\n'\n  \
         printf '{{\"mergeable\":{mergeable},\"head\":{{\"sha\":\"%040x\"}}}}\\n' \"$n\"\n  \
         exit 0 ;;\nesac\n"
    )
}
