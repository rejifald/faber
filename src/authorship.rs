//! Whose code a workflow run carries (ADR-0059 §3), read from GitHub with
//! faber's own token for [`crate::gate::trust_verdict`] to judge.
//!
//! | Event | Whose code it is |
//! |---|---|
//! | `pull_request` | the author of its pull request, fork or not |
//! | `merge_group`, or a `push` to `gh-readonly-queue/…` | the authors of the queue entry whose head commit is the run's head SHA, and of every entry ahead of it |
//! | any other event under the ceiling | the repository's own |
//! | an event outside the ceiling | its actor's and its triggering actor's |
//!
//! A merge-queue run is bound to its entry by commit, never by its branch name
//! or the queue's order. When no live entry carries the run's head SHA, the
//! group has left the queue: if the base branch contains the commit it merged,
//! and its code is the repository's own; otherwise it was destroyed, and GitHub
//! keeps running it with nobody's say on what it carries.
//!
//! What faber cannot read, it refuses ([`Code::Unread`]), with one exception: on
//! a private repository whose token lacks a read, faber keeps the verdict it
//! gave before ADR-0059 ([`Code::Ungranted`]). A read that failed in a way that
//! says nothing about the run, such as GitHub unreachable, a `5xx` or a rate
//! limit, defers the run to the next poll ([`Deferred`]).

use std::collections::HashMap;

use crate::credential::{is_permission_refusal, Permission};
use crate::gate::{Code, Person, MAX_EVENTS, QUEUE_BRANCH_PREFIX};
use crate::github::{Github, GithubError, Pull, QueueEntry, WorkflowRun};

/// The GitHub reads that say whose code a run carries. [`Github`] makes them
/// against GitHub; the tests make them against an in-house stand-in.
pub(crate) trait Reads {
    async fn pull(&self, repo: &str, number: u64) -> Result<Pull, GithubError>;
    async fn pulls_from(
        &self,
        repo: &str,
        head_owner: &str,
        branch: &str,
    ) -> Result<Vec<Pull>, GithubError>;
    async fn is_collaborator(&self, repo: &str, login: &str) -> Result<bool, GithubError>;
    async fn merge_queue(&self, repo: &str, branch: &str) -> Result<Vec<QueueEntry>, GithubError>;
    async fn branch_contains(
        &self,
        repo: &str,
        branch: &str,
        sha: &str,
    ) -> Result<bool, GithubError>;
    /// Whether GitHub refused the token `permission` on `repo` lately, so a
    /// read needing it is not tried again yet.
    fn blocked(&self, repo: &str, permission: Permission) -> bool;
}

impl Reads for Github {
    async fn pull(&self, repo: &str, number: u64) -> Result<Pull, GithubError> {
        Github::pull(self, repo, number).await
    }
    async fn pulls_from(
        &self,
        repo: &str,
        head_owner: &str,
        branch: &str,
    ) -> Result<Vec<Pull>, GithubError> {
        Github::pulls_from(self, repo, head_owner, branch).await
    }
    async fn is_collaborator(&self, repo: &str, login: &str) -> Result<bool, GithubError> {
        Github::is_collaborator(self, repo, login).await
    }
    async fn merge_queue(&self, repo: &str, branch: &str) -> Result<Vec<QueueEntry>, GithubError> {
        Github::merge_queue(self, repo, branch).await
    }
    async fn branch_contains(
        &self,
        repo: &str,
        branch: &str,
        sha: &str,
    ) -> Result<bool, GithubError> {
        Github::branch_contains(self, repo, branch, sha).await
    }
    fn blocked(&self, repo: &str, permission: Permission) -> bool {
        Github::blocked(self, repo, permission)
    }
}

/// A run to judge on the next poll instead: GitHub's answer said nothing about
/// whose code it carries. The text is why, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Deferred(pub String);

/// Whose code `run` carries when no read is needed: the repository's own, for
/// every event under the ceiling but a pull request's and a merge queue's.
pub(crate) fn without_reads(run: &WorkflowRun) -> Option<Code> {
    matches!(kind_of(run), Kind::Own).then_some(Code::Own)
}

/// Whose code each of `runs` carries, keyed by run id, reading each merge queue
/// once however many of its runs there are.
pub(crate) async fn codes_of<'a, R: Reads>(
    reads: &R,
    runs: impl IntoIterator<Item = &'a WorkflowRun>,
) -> HashMap<u64, Result<Code, Deferred>> {
    let mut queues = Queues::default();
    let mut out = HashMap::new();
    for run in runs {
        out.insert(run.id, code_with(reads, run, &mut queues).await);
    }
    out
}

/// What a run's event says to read.
enum Kind<'a> {
    Own,
    Pull,
    /// A merge group's run, for the queue of this base branch: `None` when its
    /// branch names none.
    Queue(Option<&'a str>),
    Outside,
}

fn kind_of(run: &WorkflowRun) -> Kind<'_> {
    let branch = run.head_branch.as_deref();
    match run.event.as_str() {
        "pull_request" => Kind::Pull,
        "merge_group" => Kind::Queue(branch.and_then(queue_base)),
        "push" if branch.is_some_and(|b| b.starts_with(QUEUE_BRANCH_PREFIX)) => {
            Kind::Queue(branch.and_then(queue_base))
        }
        event if MAX_EVENTS.contains(&event) => Kind::Own,
        _ => Kind::Outside,
    }
}

/// The base branch a merge queue's temporary branch is for:
/// `gh-readonly-queue/<base>/pr-<number>-<sha>` names `<base>`, which may hold
/// slashes of its own. `None` for any other shape.
fn queue_base(branch: &str) -> Option<&str> {
    let (base, tail) = branch
        .strip_prefix(QUEUE_BRANCH_PREFIX)?
        .rsplit_once("/pr-")?;
    let (number, sha) = tail.split_once('-')?;
    let digits = |s: &str, ok: fn(&u8) -> bool| !s.is_empty() && s.bytes().all(|b| ok(&b));
    (!base.is_empty() && digits(number, u8::is_ascii_digit) && digits(sha, u8::is_ascii_hexdigit))
        .then_some(base)
}

/// How a read ended the search early.
#[derive(Debug, Clone)]
enum Stop {
    /// It settled the answer: refused as unread, or kept as today.
    Code(Code),
    /// It said nothing: the run waits for the next poll.
    Deferred(String),
}

fn unread(why: impl Into<String>) -> Stop {
    Stop::Code(Code::Unread(why.into()))
}

/// What a read GitHub refused the token `permission` for comes to: today's
/// verdict on a private repository, and a refusal on a public one, which is
/// read without that grant.
fn ungranted(private: bool, permission: Permission) -> Stop {
    if private {
        Stop::Code(Code::Ungranted(permission))
    } else {
        unread(format!(
            "GitHub refused {} on a public repository",
            permission.label()
        ))
    }
}

/// What a failed read means for the search. `permission` is the grant the read
/// needs, when the token may lack it: a collaborator check needs only
/// Metadata: read, which every fine-grained token holds.
fn stop(e: GithubError, private: bool, permission: Option<Permission>) -> Stop {
    match &e {
        GithubError::Status { status, body, .. } if is_permission_refusal(*status, false, body) => {
            match permission {
                Some(permission) => ungranted(private, permission),
                None => unread(e.to_string()),
            }
        }
        GithubError::Http(_) | GithubError::Token(_) | GithubError::StaleCache(_) => {
            Stop::Deferred(e.to_string())
        }
        // Unauthorised, rate-limited or failing: nothing about the run.
        GithubError::Status { status, .. } if matches!(*status, 401 | 403 | 429 | 500..) => {
            Stop::Deferred(e.to_string())
        }
        _ => unread(e.to_string()),
    }
}

/// A read GitHub refused `permission` for lately is not tried again yet: its
/// answer is taken as given.
fn try_read<R: Reads>(
    reads: &R,
    repo: &str,
    private: bool,
    permission: Permission,
) -> Result<(), Stop> {
    if reads.blocked(repo, permission) {
        Err(ungranted(private, permission))
    } else {
        Ok(())
    }
}

/// Each merge queue read once per pass: the runs of one group share it.
#[derive(Default)]
struct Queues(HashMap<(String, String), Result<Vec<QueueEntry>, Stop>>);

impl Queues {
    async fn read<R: Reads>(
        &mut self,
        reads: &R,
        repo: &str,
        base: &str,
        private: bool,
    ) -> Result<Vec<QueueEntry>, Stop> {
        let key = (repo.to_string(), base.to_string());
        if let Some(read) = self.0.get(&key) {
            return read.clone();
        }
        let read = reads
            .merge_queue(repo, base)
            .await
            .map_err(|e| stop(e, private, Some(Permission::MergeQueuesRead)));
        self.0.insert(key, read.clone());
        read
    }
}

async fn code_with<R: Reads>(
    reads: &R,
    run: &WorkflowRun,
    queues: &mut Queues,
) -> Result<Code, Deferred> {
    let found = match kind_of(run) {
        Kind::Own => Ok(Code::Own),
        Kind::Pull => pull_code(reads, run).await,
        Kind::Queue(Some(base)) => queue_code(reads, run, base, queues).await,
        Kind::Queue(None) => Err(unread("the run's branch names no merge queue")),
        Kind::Outside => actors_code(reads, run).await,
    };
    match found {
        Ok(code) | Err(Stop::Code(code)) => Ok(code),
        Err(Stop::Deferred(why)) => Err(Deferred(why)),
    }
}

/// A `pull_request` run: the author of each pull request it names, or, for a
/// fork's, which names none, of the pull requests found from its head.
async fn pull_code<R: Reads>(reads: &R, run: &WorkflowRun) -> Result<Code, Stop> {
    let repo = run.repository.full_name.as_str();
    let private = run.repository.private;
    try_read(reads, repo, private, Permission::PullRequestsRead)?;
    let pulls = if run.pull_requests.is_empty() {
        pulls_at_head(reads, run).await?
    } else {
        let mut pulls = Vec::new();
        for named in &run.pull_requests {
            let pull = reads
                .pull(repo, named.number)
                .await
                .map_err(|e| stop(e, private, Some(Permission::PullRequestsRead)))?;
            pulls.push(pull);
        }
        pulls
    };
    let mut people = Vec::new();
    for pull in pulls {
        let login = pull
            .user
            .map(|u| u.login)
            .ok_or_else(|| unread(format!("pull request #{} names no author", pull.number)))?;
        people.push(person(reads, repo, login).await?);
    }
    Ok(Code::People(people))
}

/// A fork's pull requests, found by the run's head repository owner and branch:
/// those whose head is the run's commit, or every one from that head when none
/// is, because the head moved on since the run.
async fn pulls_at_head<R: Reads>(reads: &R, run: &WorkflowRun) -> Result<Vec<Pull>, Stop> {
    let owner = run
        .head_repository
        .as_ref()
        .and_then(|r| r.full_name.split_once('/'))
        .map(|(owner, _)| owner)
        .ok_or_else(|| unread("the run names no head repository"))?;
    let branch = run
        .head_branch
        .as_deref()
        .ok_or_else(|| unread("the run names no head branch"))?;
    let listed = reads
        .pulls_from(&run.repository.full_name, owner, branch)
        .await
        .map_err(|e| {
            stop(
                e,
                run.repository.private,
                Some(Permission::PullRequestsRead),
            )
        })?;
    if listed.is_empty() {
        return Err(unread(format!(
            "no pull request has the run's head, {owner}:{branch}"
        )));
    }
    let at_head: Vec<Pull> = listed
        .iter()
        .filter(|p| p.head.sha.eq_ignore_ascii_case(&run.head_sha))
        .cloned()
        .collect();
    Ok(if at_head.is_empty() { listed } else { at_head })
}

/// A merge group's run: bound by commit to its live entry and the entries ahead
/// of it, or, with no entry left carrying the commit, merged or destroyed.
async fn queue_code<R: Reads>(
    reads: &R,
    run: &WorkflowRun,
    base: &str,
    queues: &mut Queues,
) -> Result<Code, Stop> {
    let repo = run.repository.full_name.as_str();
    let private = run.repository.private;
    try_read(reads, repo, private, Permission::MergeQueuesRead)?;
    let mut entries = queues.read(reads, repo, base, private).await?;
    entries.sort_by_key(|e| e.position);
    let bound = entries.iter().position(|e| {
        e.head_sha
            .as_deref()
            .is_some_and(|sha| sha.eq_ignore_ascii_case(&run.head_sha))
    });
    if let Some(bound) = bound {
        let mut people = Vec::new();
        for entry in &entries[..=bound] {
            let login = entry.author.clone().ok_or_else(|| {
                unread(format!(
                    "merge queue entry {} names no author",
                    entry.position
                ))
            })?;
            people.push(person(reads, repo, login).await?);
        }
        return Ok(Code::People(people));
    }
    try_read(reads, repo, private, Permission::ContentsRead)?;
    match reads.branch_contains(repo, base, &run.head_sha).await {
        Ok(true) => Ok(Code::Own),
        Ok(false) => Ok(Code::DestroyedGroup),
        Err(e) => Err(stop(e, private, Some(Permission::ContentsRead))),
    }
}

/// An event outside the ceiling: its actor's and triggering actor's code.
async fn actors_code<R: Reads>(reads: &R, run: &WorkflowRun) -> Result<Code, Stop> {
    let repo = run.repository.full_name.as_str();
    let mut logins: Vec<String> = [&run.actor, &run.triggering_actor]
        .into_iter()
        .flatten()
        .map(|a| a.login.clone())
        .filter(|l| !l.is_empty())
        .collect();
    logins.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    if logins.is_empty() {
        return Err(unread("the run names no actor"));
    }
    let mut people = Vec::new();
    for login in logins {
        people.push(person(reads, repo, login).await?);
    }
    Ok(Code::People(people))
}

/// `login`'s standing on `repo`. An app (`[bot]`) is never a third party, and
/// neither is the repository's owner; anyone else is one unless GitHub's
/// collaborator record says otherwise.
async fn person<R: Reads>(reads: &R, repo: &str, login: String) -> Result<Person, Stop> {
    let owner = repo.split_once('/').map_or(repo, |(owner, _)| owner);
    let third_party = if login.ends_with("[bot]") || login.eq_ignore_ascii_case(owner) {
        false
    } else {
        !reads
            .is_collaborator(repo, &login)
            .await
            .map_err(|e| stop(e, true, None))?
    };
    Ok(Person { login, third_party })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};
    use std::sync::Mutex;

    use super::*;
    use crate::gate::{
        trust_verdict, JobContext, RefuseReason, TrustPolicy, Verdict, JOB_TOKEN_ACTOR,
    };
    use crate::github::{Actor, MinimalRepo, PullRequestRef, PullTip};

    /// An in-house stand-in for GitHub's side of these reads. Anything it was
    /// not told about answers `404`, a permission in `refused` answers GitHub's
    /// own `403` for a fine-grained token, and `down` makes every read a `502`.
    #[derive(Default)]
    struct FakeGithub {
        pulls: HashMap<u64, Pull>,
        /// `(head owner, branch)` to the pull requests from that head.
        heads: HashMap<(String, String), Vec<Pull>>,
        collaborators: HashSet<String>,
        /// Base branch to its queue's entries.
        queues: HashMap<String, Vec<QueueEntry>>,
        /// `(base, sha)` the base branch contains.
        contains: HashSet<(String, String)>,
        refused: BTreeSet<Permission>,
        down: bool,
        /// Every read made, in order.
        log: Mutex<Vec<String>>,
    }

    impl FakeGithub {
        fn answer<T>(
            &self,
            read: String,
            needs: Option<Permission>,
            found: Option<T>,
        ) -> Result<T, GithubError> {
            self.log.lock().unwrap().push(read.clone());
            let status = |status: u16, body: &str| GithubError::Status {
                status,
                url: read.clone(),
                body: body.to_string(),
            };
            if self.down {
                return Err(status(502, r#"{"message":"Bad Gateway"}"#));
            }
            if needs.is_some_and(|p| self.refused.contains(&p)) {
                return Err(status(
                    403,
                    r#"{"message":"Resource not accessible by personal access token"}"#,
                ));
            }
            found.ok_or_else(|| status(404, r#"{"message":"Not Found"}"#))
        }

        fn reads(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Reads for FakeGithub {
        async fn pull(&self, repo: &str, number: u64) -> Result<Pull, GithubError> {
            let found = self.pulls.get(&number).cloned();
            self.answer(
                format!("pull {repo}#{number}"),
                Some(Permission::PullRequestsRead),
                found,
            )
        }
        async fn pulls_from(
            &self,
            repo: &str,
            head_owner: &str,
            branch: &str,
        ) -> Result<Vec<Pull>, GithubError> {
            let key = (head_owner.to_string(), branch.to_string());
            let found = Some(self.heads.get(&key).cloned().unwrap_or_default());
            self.answer(
                format!("pulls_from {repo} {head_owner}:{branch}"),
                Some(Permission::PullRequestsRead),
                found,
            )
        }
        async fn is_collaborator(&self, repo: &str, login: &str) -> Result<bool, GithubError> {
            let found = Some(self.collaborators.contains(login));
            self.answer(format!("collaborator {repo} {login}"), None, found)
        }
        async fn merge_queue(
            &self,
            repo: &str,
            branch: &str,
        ) -> Result<Vec<QueueEntry>, GithubError> {
            let found = Some(self.queues.get(branch).cloned().unwrap_or_default());
            self.answer(
                format!("merge_queue {repo} {branch}"),
                Some(Permission::MergeQueuesRead),
                found,
            )
        }
        async fn branch_contains(
            &self,
            repo: &str,
            branch: &str,
            sha: &str,
        ) -> Result<bool, GithubError> {
            let found = Some(
                self.contains
                    .contains(&(branch.to_string(), sha.to_string())),
            );
            self.answer(
                format!("branch_contains {repo} {branch} {sha}"),
                Some(Permission::ContentsRead),
                found,
            )
        }
        fn blocked(&self, _repo: &str, _permission: Permission) -> bool {
            false
        }
    }

    fn sha(n: u64) -> String {
        format!("{n:040x}")
    }

    fn pull(number: u64, author: &str, head: &str) -> Pull {
        Pull {
            number,
            user: Some(Actor {
                login: author.to_string(),
            }),
            head: PullTip {
                sha: head.to_string(),
            },
            state: None,
            mergeable: None,
            closed_at: None,
        }
    }

    fn entry(position: u64, head: &str, author: &str) -> QueueEntry {
        QueueEntry {
            position,
            head_sha: Some(head.to_string()),
            pull: Some(100 + position),
            author: Some(author.to_string()),
        }
    }

    fn actor(login: &str) -> Option<Actor> {
        Some(Actor {
            login: login.to_string(),
        })
    }

    /// A run of `event` in the private `owner/demo` at commit `sha(1)`, by its
    /// owner.
    fn run(event: &str) -> WorkflowRun {
        WorkflowRun {
            id: 1,
            event: event.to_string(),
            workflow_id: 1,
            name: None,
            status: None,
            actor: actor("owner"),
            triggering_actor: actor("owner"),
            repository: MinimalRepo {
                full_name: "owner/demo".to_string(),
                fork: false,
                private: true,
            },
            head_repository: Some(MinimalRepo {
                full_name: "owner/demo".to_string(),
                fork: false,
                private: true,
            }),
            head_branch: Some("feature".to_string()),
            pull_requests: Vec::new(),
            head_sha: sha(1),
            run_attempt: 1,
            updated_at: None,
            created_at: None,
        }
    }

    /// A merge queue's run for `main` at `head`: a `merge_group` run, or the
    /// `push` to its temporary branch.
    fn queue_run(event: &str, head: &str) -> WorkflowRun {
        WorkflowRun {
            head_branch: Some(format!("gh-readonly-queue/main/pr-7-{}", sha(99))),
            head_sha: head.to_string(),
            actor: actor("github-merge-queue[bot]"),
            triggering_actor: actor("github-merge-queue[bot]"),
            head_repository: None,
            ..run(event)
        }
    }

    async fn read(gh: &FakeGithub, run: &WorkflowRun) -> Result<Code, Deferred> {
        codes_of(gh, [run]).await.remove(&run.id).unwrap()
    }

    /// The default daemon policy: provenance trust, Dependabot denied, every
    /// event under the ceiling.
    fn policy() -> TrustPolicy {
        TrustPolicy::new(
            [],
            ["dependabot[bot]".to_string()],
            vec!["faber".to_string()],
        )
        .with_boundary(MAX_EVENTS.iter().map(|e| e.to_string()), false)
    }

    /// The gate's verdict on `run` with the code read for it.
    fn verdict(run: &WorkflowRun, code: Code) -> Verdict {
        let ctx = JobContext {
            event: run.event.clone(),
            actor: run
                .actor
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_default(),
            triggering_actor: run.triggering_actor.as_ref().map(|a| a.login.clone()),
            base_repo: run.repository.full_name.clone(),
            private: run.repository.private,
            head_repo: run.head_repository.as_ref().map(|r| r.full_name.clone()),
            code,
            labels: vec!["faber".to_string()],
            status: "queued".to_string(),
        };
        trust_verdict(&ctx, &policy())
    }

    // --- refused --------------------------------------------------------------

    #[tokio::test]
    async fn a_dependabot_pr_a_maintainer_updated_is_refused_under_the_default_deny_list() {
        let mut gh = FakeGithub::default();
        gh.pulls.insert(12, pull(12, "dependabot[bot]", &sha(1)));
        let mut run = run("pull_request");
        run.actor = actor("maintainer");
        run.triggering_actor = actor("maintainer");
        run.head_branch = Some("dependabot/npm/left-pad-2".to_string());
        run.pull_requests = vec![PullRequestRef { number: 12 }];
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(
            code,
            Code::People(vec![Person {
                login: "dependabot[bot]".into(),
                third_party: false
            }])
        );
        // An app is judged by the actor lists, never asked about as a person.
        assert_eq!(gh.reads(), ["pull owner/demo#12"]);
        assert_eq!(
            verdict(&run, code),
            Verdict::Refuse(RefuseReason::UntrustedActor("dependabot[bot]".into()))
        );
    }

    #[tokio::test]
    async fn a_third_partys_fork_pr_is_refused() {
        let mut gh = FakeGithub::default();
        // The run names no pull request: it is found by its head, and the one
        // at the run's commit is judged, not an older one from the same branch.
        gh.heads.insert(
            ("a-stranger".into(), "patch-1".into()),
            vec![
                pull(3, "a-collaborator", &sha(50)),
                pull(4, "a-stranger", &sha(1)),
            ],
        );
        gh.collaborators.insert("a-collaborator".into());
        let mut run = run("pull_request");
        run.actor = actor("a-stranger");
        run.triggering_actor = actor("a-stranger");
        run.head_branch = Some("patch-1".into());
        run.head_repository = Some(MinimalRepo {
            full_name: "a-stranger/demo".into(),
            fork: true,
            private: false,
        });
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(
            code,
            Code::People(vec![Person {
                login: "a-stranger".into(),
                third_party: true
            }])
        );
        assert_eq!(
            verdict(&run, code),
            Verdict::Refuse(RefuseReason::ThirdParty("a-stranger".into()))
        );
    }

    #[tokio::test]
    async fn a_merge_group_carrying_a_strangers_pr_is_refused() {
        let mut gh = FakeGithub::default();
        gh.queues.insert(
            "main".into(),
            vec![entry(2, &sha(2), "a-stranger"), entry(1, &sha(1), "alice")],
        );
        gh.collaborators.insert("alice".into());
        let run = queue_run("merge_group", &sha(2));
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(
            verdict(&run, code),
            Verdict::Refuse(RefuseReason::ThirdParty("a-stranger".into()))
        );
    }

    #[tokio::test]
    async fn a_destroyed_group_is_refused() {
        let mut gh = FakeGithub::default();
        // The stranger's PR left the queue; the group built on it is gone, and
        // main never took its commit.
        gh.queues
            .insert("main".into(), vec![entry(1, &sha(3), "alice")]);
        gh.collaborators.insert("alice".into());
        let run = queue_run("merge_group", &sha(2));
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(code, Code::DestroyedGroup);
        assert_eq!(
            gh.reads().last().unwrap(),
            &format!("branch_contains owner/demo main {}", sha(2))
        );
        assert_eq!(
            verdict(&run, code),
            Verdict::Refuse(RefuseReason::DestroyedGroup)
        );
    }

    #[tokio::test]
    async fn a_queue_branch_push_carrying_a_strangers_pr_is_refused() {
        let mut gh = FakeGithub::default();
        gh.queues
            .insert("main".into(), vec![entry(1, &sha(4), "a-stranger")]);
        let run = queue_run("push", &sha(4));
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(
            verdict(&run, code),
            Verdict::Refuse(RefuseReason::ThirdParty("a-stranger".into()))
        );
    }

    #[tokio::test]
    async fn a_job_token_workflow_dispatch_is_refused_with_no_read() {
        let gh = FakeGithub::default();
        let mut run = run("workflow_dispatch");
        run.actor = actor(JOB_TOKEN_ACTOR);
        run.triggering_actor = actor(JOB_TOKEN_ACTOR);
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(code, Code::Own);
        assert!(gh.reads().is_empty());
        assert_eq!(
            verdict(&run, code),
            Verdict::Refuse(RefuseReason::JobTokenDispatch)
        );
    }

    #[tokio::test]
    async fn a_public_repo_whose_token_lacks_the_read_is_refused() {
        // A public repository's pull requests are read without the grant, so a
        // refusal there is not a missing box: what faber cannot read, it refuses.
        let mut gh = FakeGithub::default();
        gh.refused.insert(Permission::PullRequestsRead);
        let mut run = run("pull_request");
        run.repository.private = false;
        run.pull_requests = vec![PullRequestRef { number: 12 }];
        let code = read(&gh, &run).await.unwrap();
        assert!(matches!(code, Code::Unread(_)), "{code:?}");
        assert_eq!(verdict(&run, code), Verdict::Refuse(RefuseReason::Unread));
    }

    #[tokio::test]
    async fn an_event_outside_the_ceiling_is_a_strangers_when_its_actor_is() {
        let mut gh = FakeGithub::default();
        gh.collaborators.insert("alice".into());
        let mut run = run("issue_comment");
        run.actor = actor("alice");
        run.triggering_actor = actor("a-stranger");
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(
            verdict(&run, code),
            Verdict::Refuse(RefuseReason::ThirdParty("a-stranger".into()))
        );
        // A collaborator's is refused as before, as an event outside the ceiling.
        run.triggering_actor = actor("alice");
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(
            verdict(&run, code),
            Verdict::Refuse(RefuseReason::DisallowedEvent("issue_comment".into()))
        );
    }

    // --- allowed --------------------------------------------------------------

    #[tokio::test]
    async fn a_merge_group_of_collaborators_prs_is_allowed_bound_by_its_head() {
        let mut gh = FakeGithub::default();
        // The stranger's entry is behind the group, so its code is not in it.
        gh.queues.insert(
            "main".into(),
            vec![
                entry(1, &sha(1), "alice"),
                entry(2, &sha(2), "owner"),
                entry(3, &sha(3), "a-stranger"),
            ],
        );
        gh.collaborators.insert("alice".into());
        let run = queue_run("merge_group", &sha(2));
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(
            code,
            Code::People(vec![
                Person {
                    login: "alice".into(),
                    third_party: false
                },
                Person {
                    login: "owner".into(),
                    third_party: false
                },
            ])
        );
        assert_eq!(verdict(&run, code), Verdict::Provision);
        // The owner needs no check, and the stranger behind it none either.
        assert!(!gh
            .reads()
            .iter()
            .any(|r| r.contains("a-stranger") || r.ends_with(" owner")));
    }

    #[tokio::test]
    async fn a_late_job_from_a_group_that_merged_is_allowed() {
        let mut gh = FakeGithub::default();
        gh.contains.insert(("main".into(), sha(2)));
        for event in ["merge_group", "push"] {
            let run = queue_run(event, &sha(2));
            let code = read(&gh, &run).await.unwrap();
            assert_eq!(code, Code::Own, "{event}");
            assert_eq!(verdict(&run, code), Verdict::Provision, "{event}");
        }
    }

    // --- kept as today ----------------------------------------------------------

    #[tokio::test]
    async fn a_private_repo_whose_token_lacks_pull_requests_read_keeps_todays_verdict() {
        let mut gh = FakeGithub::default();
        gh.refused.insert(Permission::PullRequestsRead);
        let mut run = run("pull_request");
        run.pull_requests = vec![PullRequestRef { number: 12 }];
        let code = read(&gh, &run).await.unwrap();
        assert_eq!(code, Code::Ungranted(Permission::PullRequestsRead));
        assert_eq!(verdict(&run, code), Verdict::Provision);
        // Its merge queue and ancestry reads alike.
        let mut gh = FakeGithub::default();
        gh.refused.insert(Permission::MergeQueuesRead);
        let run = queue_run("merge_group", &sha(2));
        assert_eq!(
            read(&gh, &run).await.unwrap(),
            Code::Ungranted(Permission::MergeQueuesRead)
        );
        let mut gh = FakeGithub::default();
        gh.refused.insert(Permission::ContentsRead);
        assert_eq!(
            read(&gh, &run).await.unwrap(),
            Code::Ungranted(Permission::ContentsRead)
        );
    }

    // --- deferred, and read once -----------------------------------------------

    #[tokio::test]
    async fn a_github_that_does_not_answer_defers_the_run() {
        let gh = FakeGithub {
            down: true,
            ..FakeGithub::default()
        };
        let mut pr = run("pull_request");
        pr.pull_requests = vec![PullRequestRef { number: 12 }];
        let mut outside = run("workflow_run");
        outside.triggering_actor = actor("alice");
        for run in [pr, queue_run("merge_group", &sha(2)), outside] {
            let got = read(&gh, &run).await;
            assert!(matches!(got, Err(Deferred(_))), "{}: {got:?}", run.event);
        }
    }

    #[tokio::test]
    async fn a_merge_queue_is_read_once_for_every_run_of_it() {
        let mut gh = FakeGithub::default();
        gh.queues
            .insert("main".into(), vec![entry(1, &sha(2), "owner")]);
        let runs: Vec<WorkflowRun> = (1..=3)
            .map(|id| WorkflowRun {
                id,
                ..queue_run("merge_group", &sha(2))
            })
            .collect();
        let codes = codes_of(&gh, &runs).await;
        assert_eq!(codes.len(), 3);
        assert!(codes.values().all(|c| matches!(c, Ok(Code::People(_)))));
        assert_eq!(gh.reads(), ["merge_queue owner/demo main"]);
    }

    #[test]
    fn a_queue_branch_names_its_base_branch() {
        let hex = sha(5);
        assert_eq!(
            queue_base(&format!("gh-readonly-queue/main/pr-12-{hex}")),
            Some("main")
        );
        assert_eq!(
            queue_base(&format!("gh-readonly-queue/release/1.x/pr-3-{hex}")),
            Some("release/1.x")
        );
        for shape in [
            "main".to_string(),
            format!("gh-readonly-queue/pr-12-{hex}"),
            "gh-readonly-queue/main/pr-x-abc".to_string(),
            "gh-readonly-queue/main/pr-12-".to_string(),
            format!("gh-readonly-queue//pr-12-{hex}"),
        ] {
            assert_eq!(queue_base(&shape), None, "{shape}");
        }
    }

    #[test]
    fn only_a_pull_request_or_a_merge_queue_or_an_outside_event_needs_a_read() {
        assert_eq!(without_reads(&run("push")), Some(Code::Own));
        assert_eq!(without_reads(&run("schedule")), Some(Code::Own));
        assert_eq!(without_reads(&run("workflow_dispatch")), Some(Code::Own));
        for needs in [
            run("pull_request"),
            run("pull_request_target"),
            queue_run("merge_group", &sha(1)),
            queue_run("push", &sha(1)),
        ] {
            assert_eq!(without_reads(&needs), None, "{}", needs.event);
        }
    }
}
