//! An in-memory forge for the liveness tests.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};

use crate::forge_listing::RestIssue;
use crate::star_liveness::forge::{ForgeComment, SearchHit, StarForge};
use crate::star_liveness::task::{LivenessState, RepoInput};
use crate::star_liveness::Settings;
use crate::types::{ReadyQueueRow, StarLivenessReport};

/// One repo's forge state.
#[derive(Debug, Default)]
pub struct Repo {
    pub items: BTreeMap<u32, RestIssue>,
    pub comments: BTreeMap<u32, Vec<ForgeComment>>,
    /// Every comment posted through the fake: (number, body).
    pub posted: Vec<(u32, String)>,
    /// Comment reads made through the fake.
    pub comment_reads: usize,
    /// Issue searches made through the fake.
    pub searches: usize,
    /// Single-issue reads made through the fake.
    pub issue_reads: usize,
    /// `author_association` of each issue's author (absent: `NONE`).
    pub associations: BTreeMap<u32, String>,
    pub fail_listing: bool,
    /// Labels whose removal fails (write-failure injection).
    pub fail_remove: Vec<String>,
    /// Make every `post_comment` fail.
    pub fail_post: bool,
    /// Native "blocked by" dependencies per issue (#10307).
    pub blocked_by: BTreeMap<u32, Vec<(String, u32)>>,
    /// Label writes made through the fake: (number, "+label" / "-label").
    pub label_writes: Vec<(u32, String)>,
}

/// A forge world: repos by slug.
#[derive(Debug, Default, Clone)]
pub struct World(pub Rc<RefCell<BTreeMap<String, Repo>>>);

impl World {
    pub fn repo(&self, slug: &str) -> std::cell::RefMut<'_, Repo> {
        std::cell::RefMut::map(self.0.borrow_mut(), |m| m.entry(slug.to_string()).or_default())
    }

    pub fn add(&self, slug: &str, item: RestIssue) {
        self.repo(slug).items.insert(item.number, item);
    }

    /// A comment by the fleet App (trusted).
    pub fn comment(&self, slug: &str, number: u32, body: &str) {
        self.comment_full(slug, number, bot(body));
    }

    /// A comment with every field chosen by the test.
    pub fn comment_full(&self, slug: &str, number: u32, c: ForgeComment) {
        self.repo(slug).comments.entry(number).or_default().push(c);
    }

    pub fn posted(&self, slug: &str) -> Vec<(u32, String)> {
        self.repo(slug).posted.clone()
    }

    pub fn forge(&self, slug: &str) -> Box<dyn StarForge> {
        Box::new(FakeForge {
            world: self.clone(),
            slug: slug.to_string(),
        })
    }
}

pub struct FakeForge {
    world: World,
    slug: String,
}

impl StarForge for FakeForge {
    fn list_open(&mut self, label: &str) -> Result<Vec<RestIssue>> {
        let repo = self.world.repo(&self.slug);
        if repo.fail_listing {
            return Err(anyhow!("listing failed"));
        }
        Ok(repo
            .items
            .values()
            .filter(|i| i.state == "open" && i.labels.iter().any(|l| l == label))
            .cloned()
            .collect())
    }

    fn issue(&mut self, number: u32) -> Result<Option<RestIssue>> {
        let mut repo = self.world.repo(&self.slug);
        repo.issue_reads += 1;
        Ok(repo.items.get(&number).cloned())
    }

    fn comments(&mut self, number: u32) -> Result<Vec<ForgeComment>> {
        let mut repo = self.world.repo(&self.slug);
        repo.comment_reads += 1;
        Ok(repo.comments.get(&number).cloned().unwrap_or_default())
    }

    fn search_open_issues(&mut self, phrase: &str) -> Result<Vec<SearchHit>> {
        let mut repo = self.world.repo(&self.slug);
        repo.searches += 1;
        let wanted = phrase.to_ascii_lowercase();
        Ok(repo
            .items
            .values()
            .filter(|i| i.state == "open" && !i.is_pull_request)
            .filter(|i| {
                format!(
                    "{}\n{}",
                    i.title.as_deref().unwrap_or_default(),
                    i.body.as_deref().unwrap_or_default()
                )
                .to_ascii_lowercase()
                .contains(&wanted)
            })
            .map(|i| SearchHit {
                issue: i.clone(),
                author_association: repo.associations.get(&i.number).cloned(),
            })
            .collect())
    }

    fn blocked_by(&mut self, number: u32) -> Result<Vec<(String, u32)>> {
        Ok(self
            .world
            .repo(&self.slug)
            .blocked_by
            .get(&number)
            .cloned()
            .unwrap_or_default())
    }

    fn add_label(&mut self, number: u32, label: &str) -> Result<()> {
        let mut repo = self.world.repo(&self.slug);
        repo.label_writes.push((number, format!("+{label}")));
        let item = repo
            .items
            .get_mut(&number)
            .ok_or_else(|| anyhow!("no #{number}"))?;
        if !item.labels.iter().any(|l| l == label) {
            item.labels.push(label.to_string());
        }
        Ok(())
    }

    fn remove_label(&mut self, number: u32, label: &str) -> Result<()> {
        let mut repo = self.world.repo(&self.slug);
        if repo.fail_remove.iter().any(|l| l == label) {
            return Err(anyhow!("removing {label} from #{number} failed"));
        }
        repo.label_writes.push((number, format!("-{label}")));
        if let Some(item) = repo.items.get_mut(&number) {
            item.labels.retain(|l| l != label);
        }
        Ok(())
    }

    fn post_comment(&mut self, number: u32, body: &str) -> Result<()> {
        let mut repo = self.world.repo(&self.slug);
        if repo.fail_post {
            return Err(anyhow!("post failed"));
        }
        repo.posted.push((number, body.to_string()));
        repo.comments.entry(number).or_default().push(bot(body));
        Ok(())
    }
}

/// A comment by the fleet App, as GitHub reports it (`CONTRIBUTOR`).
pub fn bot(body: &str) -> ForgeComment {
    ForgeComment {
        body: body.to_string(),
        author: Some("loom-fleet-dispatch-1[bot]".into()),
        author_association: Some("CONTRIBUTOR".into()),
        ..ForgeComment::default()
    }
}

/// A comment by an outside account (untrusted).
pub fn outsider(body: &str) -> ForgeComment {
    ForgeComment {
        body: body.to_string(),
        author: Some("drive-by".into()),
        author_association: Some("NONE".into()),
        ..ForgeComment::default()
    }
}

/// An open issue.
pub fn issue(number: u32, labels: &[&str]) -> RestIssue {
    RestIssue {
        comments: 0,
        number,
        title: Some(format!("issue {number}")),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some(format!("2026-09-{:02}T00:00:00Z", 1 + number % 27)),
        updated_at: Some("2026-09-28T00:00:00Z".to_string()),
        closed_at: None,
        state: "open".to_string(),
        body: Some(String::new()),
        author: None,
        is_pull_request: false,
    }
}

/// An issue with a body.
pub fn issue_with_body(number: u32, labels: &[&str], body: &str) -> RestIssue {
    RestIssue {
        body: Some(body.to_string()),
        ..issue(number, labels)
    }
}

/// An open PR closing `closes`.
pub fn pr(number: u32, closes: u32, labels: &[&str]) -> RestIssue {
    RestIssue {
        body: Some(format!("Summary.\n\nCloses #{closes}\n")),
        is_pull_request: true,
        ..issue(number, labels)
    }
}

pub const STAR: &str = "loom:operator-priority";

pub fn t(h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 28, h, m, 0).unwrap()
}

pub fn repo_input(slug: &str) -> RepoInput {
    RepoInput {
        root: PathBuf::from(format!("/nonexistent/star-liveness-tests/{slug}")),
        slug: slug.to_string(),
        tick_rows: Vec::new(),
        pool: None,
        host_queue: std::sync::Arc::default(),
        cap: None,
        web_base: crate::star_liveness::task::DEFAULT_WEB_BASE.to_string(),
    }
}

pub fn settings() -> Settings {
    Settings {
        escalate: true,
        ..Settings::default()
    }
}

/// One host.
pub struct Host {
    pub id: String,
    pub state: LivenessState,
    /// Every fleet-comms notice this host's passes produced (#9321), in order
    /// — the test-side stand-in for the event-bus publish `task::spawn` does.
    pub notices: Vec<crate::star_liveness::escalate::Notice>,
}

impl Host {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            state: LivenessState::default(),
            notices: Vec::new(),
        }
    }

    /// Simulate a daemon restart: the process-lifetime ledger is gone, the
    /// forge (the `World`) is not.
    pub fn restart(&mut self) {
        self.state = LivenessState::default();
    }

    /// The bus events this host's notices would publish (#9321).
    pub fn events(&self) -> Vec<crate::types::Event> {
        self.notices.iter().map(|n| n.to_event(&self.id)).collect()
    }

    pub fn pass(
        &mut self,
        world: &World,
        repos: &[RepoInput],
        intents: Vec<crate::star_liveness::intents::StarIntent>,
        now: DateTime<Utc>,
    ) -> StarLivenessReport {
        self.pass_with(world, repos, intents, now, settings())
    }

    pub fn pass_with(
        &mut self,
        world: &World,
        repos: &[RepoInput],
        intents: Vec<crate::star_liveness::intents::StarIntent>,
        now: DateTime<Utc>,
        settings: Settings,
    ) -> StarLivenessReport {
        let w = world.clone();
        let mut factory = move |_root: &Path, slug: &str| w.forge(slug);
        let report = self
            .state
            .run_pass(repos, intents, settings, &self.id, now, &mut factory);
        self.notices.extend(self.state.take_notices());
        report
    }
}

/// A tick row for `issue` with `disposition`.
pub fn tick_row(
    root: &Path,
    issue: u32,
    disposition: crate::types::QueueDisposition,
) -> ReadyQueueRow {
    ReadyQueueRow {
        rank: 1,
        repo: root.display().to_string(),
        issue,
        workspace_priority: 100,
        urgent: false,
        operator_priority: true,
        operator_priority_at: None,
        main_red_fix: false,
        created_at: None,
        tier: None,
        story_points: None,
        disposition,
        detail: None,
        state: disposition.state().to_string(),
        reason: disposition.reason().to_string(),
        plan: crate::types::RowPlan::default(),
    }
}
