//! The daemon's side of Claims: who is calling, claiming and releasing on
//! GitHub without two Sessions racing, and ending Claims whose issue closed.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{ATTESTATION_WAIT, Daemon, env_ms, lock};
use crate::attest::Destination;
use crate::claim::{self, Claim, IssueRef};
use crate::git;
use crate::paths;
use crate::protocol::Reply;
use crate::session::SessionKey;
use crate::store::{Store, now_ms};

/// How often each Claim's issue is checked, so a closed issue ends its Claim.
/// `GELEIDER_CLAIM_CHECK_MS` overrides it for tests.
const CLAIM_CHECK_INTERVAL: Duration = Duration::from_secs(120);

/// An issue held for a claim that is under way on GitHub, so a second claim
/// fails at once instead of racing it. Dropping it ends the hold, whether the
/// claim succeeded, failed or panicked.
struct Reservation<'a> {
    daemon: &'a Daemon,
    key: (String, u64),
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        lock(&self.daemon.core).claiming.remove(&self.key);
    }
}

impl Daemon {
    /// The Session making a claim or release call: the one a hook attested, or
    /// else the only live Session in `worktree`. `Err` says why there is none.
    fn caller(
        &self,
        session: Option<Destination>,
        worktree: Option<&Path>,
    ) -> Result<SessionKey, String> {
        if let Some(key) = session
            .filter(|d| self.attestations.consume(d, ATTESTATION_WAIT))
            .map(|d| d.key)
        {
            return Ok(key);
        }
        let Some(worktree) = worktree else {
            return Err("this directory is not in a configured Project".into());
        };
        let live = lock(&self.core)
            .store
            .live_sessions()
            .map_err(|err| format!("{err:#}"))?;
        let here: Vec<_> = live
            .into_iter()
            .filter(|s| s.worktree == worktree)
            .collect();
        match here.as_slice() {
            [only] => Ok(only.key.clone()),
            [] => Err(format!(
                "geleider tracks no Session in {}",
                paths::display(worktree)
            )),
            many => Err(format!(
                "{} Sessions are working in {} ({})",
                many.len(),
                paths::display(worktree),
                many.iter()
                    .map(|s| format!("{} {}", s.key.harness.as_str(), s.key.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    pub(super) fn claim(
        &self,
        cwd: &Path,
        issue: &IssueRef,
        session: Option<Destination>,
    ) -> Result<Reply> {
        // Locating the worktree runs git, so it happens without holding the lock.
        let config = self.config()?;
        let location = git::locate(&config, cwd)?;
        let repo = location.project.repo.clone();
        if let Some(named) = issue.repo.as_ref().filter(|r| **r != repo) {
            bail!(
                "{} is in project `{}` ({repo}), not {named}; claim from a worktree of {named}",
                paths::display(&location.worktree),
                location.project_name
            );
        }
        let holder = self
            .caller(session, Some(&location.worktree))
            .map_err(|why| {
                anyhow::anyhow!(
                    "can't tell which Session is claiming: {why}. Claim with the MCP tool `claim`, \
                 whose geleider hook names the calling Session (see `geleider install`)."
                )
            })?;
        let new = Claim {
            repo: repo.clone(),
            issue: issue.number,
            project: location.project_name.to_string(),
            session: holder,
            worktree: location.worktree.clone(),
            branch: location.branch.clone(),
            git_dir: location.common_dir.clone(),
            assignee: String::new(),
            claimed_at: now_ms(),
        };
        let reservation = {
            let mut core = lock(&self.core);
            if let Some(held) = core.store.claim(&repo, issue.number)? {
                if held.session == new.session {
                    let message = format!("{} is already claimed by this Session.", held.name());
                    return Ok(Reply::Claim {
                        claim: held,
                        message,
                    });
                }
                bail!(claim::held_by(&held));
            }
            let key = (repo, issue.number);
            if !core.claiming.insert(key.clone()) {
                bail!(
                    "{} is being claimed by another Session right now",
                    new.name()
                );
            }
            Reservation { daemon: self, key }
        };
        // `gh` is slow, so it runs without holding the lock; the reservation
        // keeps the issue to this claim meanwhile, and ends however it ends.
        let claimed = claim::claim_on_github(self.github.as_ref(), new)?;
        let core = lock(&self.core);
        core.store.insert_claim(&claimed)?;
        self.publish_claims(&core.store);
        drop(core);
        drop(reservation);
        let message = format!(
            "Claimed {}: assigned to @{} with a comment naming this Session. It holds the Claim \
             until the issue closes or it is released (`release`).",
            claimed.name(),
            claimed.assignee
        );
        Ok(Reply::Claim {
            claim: claimed,
            message,
        })
    }

    pub(super) fn release(
        &self,
        cwd: &Path,
        issue: &IssueRef,
        session: Option<Destination>,
        force: bool,
    ) -> Result<Reply> {
        let config = self.config()?;
        let location = git::locate(&config, cwd);
        let repo = match (&issue.repo, &location) {
            (Some(repo), _) => repo.clone(),
            (None, Ok(location)) => location.project.repo.clone(),
            (None, Err(err)) => bail!("{err:#}; name the issue as owner/name#{}", issue.number),
        };
        let worktree = location.as_ref().ok().map(|l| l.worktree.as_path());
        let caller = self.caller(session, worktree);
        let held = {
            let core = lock(&self.core);
            let held = core
                .store
                .claim(&repo, issue.number)?
                .with_context(|| format!("no Session holds a Claim on {repo}#{}", issue.number))?;
            if !force && caller.as_ref() != Ok(&held.session) {
                bail!(
                    "{} is held by {}; only that Session can release it. The user can end \
                     it anyway from a terminal with `geleider release {} --force`.",
                    held.name(),
                    held.holder(),
                    held.name()
                );
            }
            // Ended here first, so a GitHub failure can't leave a Claim nobody can end.
            core.store.end_claim(&held)?;
            self.publish_claims(&core.store);
            held
        };
        let message = match claim::release_on_github(self.github.as_ref(), &held) {
            Ok(()) => format!(
                "Released {}: unassigned @{} and commented that the Claim was released.",
                held.name(),
                held.assignee
            ),
            Err(err) => format!(
                "Released {} in geleider, but GitHub was not updated ({err:#}). \
                 Unassign @{} there by hand.",
                held.name(),
                held.assignee
            ),
        };
        Ok(Reply::Claim {
            claim: held,
            message,
        })
    }

    /// Ends the Claims whose issue has closed. Runs in its own thread since `gh`
    /// can be slow; a Claim is never ended for lack of activity.
    pub(super) fn claims_loop(&self) {
        let interval = env_ms("GELEIDER_CLAIM_CHECK_MS").unwrap_or(CLAIM_CHECK_INTERVAL);
        loop {
            std::thread::sleep(interval);
            let claims = match lock(&self.core).store.claims() {
                Ok(claims) => claims,
                Err(err) => {
                    eprintln!("geleider: reading Claims: {err:#}");
                    continue;
                }
            };
            for held in claims {
                match self.github.issue(&held.repo, held.issue) {
                    Ok(issue) if !issue.open => {
                        let core = lock(&self.core);
                        match core.store.end_claim(&held) {
                            Ok(true) => {
                                self.publish_claims(&core.store);
                                eprintln!("geleider: {} closed; its Claim ended", held.name());
                            }
                            Ok(false) => {}
                            Err(err) => eprintln!("geleider: ending {}: {err:#}", held.name()),
                        }
                    }
                    Ok(_) => {}
                    Err(err) => eprintln!("geleider: checking {}: {err:#}", held.name()),
                }
            }
        }
    }

    /// Rewrites the Claims snapshot start hooks read. Called with the lock held,
    /// so snapshots are written in the order the Claims changed.
    pub(super) fn publish_claims(&self, store: &Store) {
        let written = store
            .claims()
            .and_then(|claims| claim::write_snapshot(&self.claims_file, &claims));
        if let Err(err) = written {
            eprintln!("geleider: writing the Claims snapshot: {err:#}");
        }
    }
}
