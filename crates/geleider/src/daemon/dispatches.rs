//! The daemon's side of Dispatch: the Claim for a Session about to start in a
//! new worktree, the id it really started under, and the notifications only
//! Dispatched Sessions send.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::{Daemon, lock};
use crate::claim::{self, Claim};
use crate::dispatch::Dispatched;
use crate::git;
use crate::paths;
use crate::protocol::Reply;
use crate::session::{Session, SessionKey, SessionState};
use crate::store::{Store, now_ms};
use crate::text;

/// What a Dispatch is about to start, as the CLI asks the daemon to claim it.
pub(super) struct Planned {
    pub cwd: PathBuf,
    pub number: u64,
    pub session: SessionKey,
    pub worktree: PathBuf,
    pub branch: String,
    pub pull_request: bool,
    pub profile: String,
}

/// A notification a Dispatched Session's activity calls for.
pub(super) enum Notice {
    Waiting(Dispatched),
    /// Its first idle or end: the branch's open PR is looked up off the lock.
    Finished(Dispatched, SessionState),
}

impl Daemon {
    /// Claims `planned.number` for a Session that does not exist yet. Only a
    /// worktree nothing uses, in the clone's `.claude/worktrees/`, can be
    /// named, so the holder is the only Session that will ever work there.
    pub(super) fn dispatch_claim(&self, planned: Planned) -> Result<Reply> {
        let config = self.config()?;
        let location = git::locate(&config, &planned.cwd)?;
        let clone = location.project.clone_path()?.canonicalize()?;
        let parent = planned
            .worktree
            .parent()
            .context("worktree has no parent")?;
        let inside = parent
            .canonicalize()
            .is_ok_and(|p| p == clone.join(".claude/worktrees"));
        if !inside || planned.worktree.exists() {
            bail!(
                "a Dispatch worktree must be a new directory in {}",
                paths::display(&clone.join(".claude/worktrees"))
            );
        }
        let worktree = parent.canonicalize()?.join(
            planned
                .worktree
                .file_name()
                .context("worktree has no name")?,
        );
        let repo = location.project.repo.clone();
        let new = Claim {
            repo: repo.clone(),
            issue: planned.number,
            project: location.project_name.to_string(),
            session: planned.session.clone(),
            worktree: worktree.clone(),
            branch: Some(planned.branch.clone()),
            git_dir: location.common_dir.clone(),
            assignee: String::new(),
            claimed_at: now_ms(),
        };
        let reservation = {
            let mut core = lock(&self.core);
            if core.store.session(&planned.session)?.is_some()
                || core.store.dispatch(&planned.session)?.is_some()
            {
                bail!("Session {} already exists", planned.session.id);
            }
            if let Some(held) = core.store.claim(&repo, planned.number)? {
                bail!(claim::held_by(&held));
            }
            self.reserve(&mut core, &repo, planned.number)?
        };
        let claimed = claim::claim_on_github(self.github.as_ref(), new)?;
        let core = lock(&self.core);
        core.store.insert_claim(&claimed)?;
        core.store.put_dispatch(&Dispatched {
            session: planned.session,
            repo,
            number: planned.number,
            pull_request: planned.pull_request,
            branch: planned.branch,
            worktree,
            profile: planned.profile,
            started: false,
            finished_notified: false,
        })?;
        self.publish_claims(&core.store);
        drop(core);
        drop(reservation);
        let message = format!("Claimed {} for the Dispatched Session.", claimed.name());
        Ok(Reply::Claim {
            claim: claimed,
            message,
        })
    }

    /// Records that the Dispatched Session started, moving its Claim to the id
    /// the Harness gave it when that differs from the planned one.
    pub(super) fn dispatch_started(&self, planned: &SessionKey, actual: &SessionKey) -> Result<()> {
        let core = lock(&self.core);
        let mut dispatch = core
            .store
            .dispatch(planned)?
            .filter(|d| !d.started)
            .with_context(|| format!("no Dispatch is starting Session {}", planned.id))?;
        if actual != planned {
            if actual.harness != planned.harness || core.store.dispatch(actual)?.is_some() {
                bail!("Session {} cannot take over this Dispatch", actual.id);
            }
            if let Some(held) = core
                .store
                .claim(&dispatch.repo, dispatch.number)?
                .filter(|c| c.session == *planned)
            {
                core.store.move_claim(&held, actual)?;
            }
            core.store.delete_dispatch(planned)?;
        }
        dispatch.session = actual.clone();
        dispatch.started = true;
        core.store.put_dispatch(&dispatch)?;
        self.publish_claims(&core.store);
        Ok(())
    }

    /// Releases the Claim of a Dispatch whose Session never started.
    pub(super) fn dispatch_failed(&self, planned: &SessionKey) -> Result<Reply> {
        let held = {
            let core = lock(&self.core);
            let dispatch = core
                .store
                .dispatch(planned)?
                .filter(|d| !d.started)
                .with_context(|| format!("no Dispatch is starting Session {}", planned.id))?;
            core.store.delete_dispatch(planned)?;
            let held = core
                .store
                .claim(&dispatch.repo, dispatch.number)?
                .filter(|c| c.session == *planned)
                .with_context(|| format!("{}#{} has no Claim", dispatch.repo, dispatch.number))?;
            core.store.end_claim(&held)?;
            self.publish_claims(&core.store);
            held
        };
        let message = match claim::release_on_github(self.github.as_ref(), &held) {
            Ok(()) => format!("Released {}.", held.name()),
            Err(err) => format!(
                "Released {} in geleider, but GitHub was not updated ({err:#}). Unassign @{} \
                 there by hand.",
                held.name(),
                held.assignee
            ),
        };
        Ok(Reply::Claim {
            claim: held,
            message,
        })
    }

    /// What, if anything, `session` changing from `was` should tell the user.
    /// Called with the lock held, so the first idle is marked only once.
    pub(super) fn dispatch_notice(
        &self,
        store: &Store,
        session: &Session,
        was: Option<SessionState>,
    ) -> Result<Option<Notice>> {
        // Not only started ones: a Session can wait or finish before the CLI
        // has recorded its start. Provisional Codex ids never report activity.
        let Some(mut dispatch) = store.dispatch(&session.key)? else {
            return Ok(None);
        };
        Ok(match session.state {
            SessionState::Waiting if was != Some(SessionState::Waiting) => {
                Some(Notice::Waiting(dispatch))
            }
            SessionState::Idle | SessionState::Ended if !dispatch.finished_notified => {
                dispatch.finished_notified = true;
                store.put_dispatch(&dispatch)?;
                Some(Notice::Finished(dispatch, session.state))
            }
            _ => None,
        })
    }

    /// Sends `notice` from its own thread, since finding the PR runs `gh`.
    pub(super) fn announce_dispatch(self: &Arc<Self>, notice: Notice) {
        let daemon = Arc::clone(self);
        std::thread::spawn(move || {
            let (title, message) = match notice {
                Notice::Waiting(d) => (
                    format!("geleider: {} is waiting", d.name()),
                    format!(
                        "The Dispatched Session on {} needs you. {}",
                        text::one_line(&d.branch),
                        d.attach()
                    ),
                ),
                Notice::Finished(d, state) => {
                    let pr = match daemon.github.open_pr(&d.repo, &d.branch) {
                        Ok(Some(pr)) => format!("PR #{pr} is open for its branch."),
                        Ok(None) => "It opened no PR.".into(),
                        Err(err) => format!("Its PR could not be checked ({err:#})."),
                    };
                    let verb = if state == SessionState::Ended {
                        "ended"
                    } else {
                        "finished its turn"
                    };
                    (
                        format!("geleider: {} {verb}", d.name()),
                        format!(
                            "{} on {}. {pr} {}",
                            d.session.harness.name(),
                            text::one_line(&d.branch),
                            d.attach()
                        ),
                    )
                }
            };
            daemon.notifier.notify(&title, &message);
        });
    }
}
