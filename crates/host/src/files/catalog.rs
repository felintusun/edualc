//! The sessions agy and opencode started elsewhere, listed once for the reads that ask at the
//! same time.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::acp::{self, Agent, ExternalSession, ListingError};

/// How long a `session/list` answer stays fresh before it is asked again.
const CATALOG_TTL: Duration = Duration::from_secs(30);

/// The way to [`keep_catalog`].
pub type Catalog = mpsc::Sender<CatalogQuery>;

/// Asks for the agy and opencode sessions started elsewhere.
#[derive(Debug)]
pub struct CatalogQuery {
    pub cached_only: bool,
    pub reply: oneshot::Sender<Vec<ExternalSession>>,
}

/// Keeps the list of agy and opencode sessions started elsewhere, as their one owner, so that
/// reads at the same time ask the programs once.
pub async fn keep_catalog(mut queries: mpsc::Receiver<CatalogQuery>) {
    let mut listing = Listing::default();
    // The listing runs as a task the catalog holds, so that even one that panics ends.
    let mut listings = JoinSet::new();
    loop {
        let steps = tokio::select! {
            query = queries.recv() => {
                let Some(CatalogQuery { cached_only, reply }) = query else {
                    return;
                };
                listing.asked(reply, cached_only, Instant::now())
            }
            Some(joined) = listings.join_next() => {
                let told = match joined {
                    Ok(told) => told,
                    Err(error) => {
                        tracing::error!(%error, "listing the agents' sessions failed");
                        Vec::new()
                    }
                };
                listing.listed(told, Instant::now())
            }
        };
        for step in steps {
            match step {
                Step::Answer(reply, sessions) => {
                    if reply.send(sessions).is_err() {
                        tracing::debug!("a read of the catalog went away");
                    }
                }
                Step::List => {
                    listings.spawn(list_all());
                }
            }
        }
    }
}

/// What each agent answered when asked for its sessions.
type Told = Vec<(Agent, Result<Vec<ExternalSession>, ListingError>)>;

/// Asks agy and opencode for their sessions.
async fn list_all() -> Told {
    let (agy, opencode) = tokio::join!(
        acp::list_sessions(Agent::Agy),
        acp::list_sessions(Agent::Opencode),
    );
    vec![(Agent::Agy, agy), (Agent::Opencode, opencode)]
}

pub(super) async fn query_catalog(catalog: &Catalog, cached_only: bool) -> Vec<ExternalSession> {
    // The catalog ends only once each sender has gone, and this read holds one.
    let (reply, answer) = oneshot::channel();
    if catalog
        .send(CatalogQuery { cached_only, reply })
        .await
        .is_err()
    {
        tracing::error!("the catalog has ended: no agy or opencode sessions listed");
        return Vec::new();
    }
    match answer.await {
        Ok(sessions) => sessions,
        Err(_ended) => {
            tracing::error!("the catalog ended before it answered: no agy or opencode sessions");
            Vec::new()
        }
    }
}

/// What the catalog knows, and who waits for its listing; each `Waiter` is answered once.
#[derive(Debug)]
struct Listing<Waiter> {
    last: Option<Listed>,
    asking: Asking<Waiter>,
}

impl<Waiter> Default for Listing<Waiter> {
    fn default() -> Self {
        Self {
            last: None,
            asking: Asking::Not,
        }
    }
}

#[derive(Debug)]
struct Listed {
    at: Instant,
    /// Each agent's sessions, as it last listed them.
    by_agent: BTreeMap<Agent, Vec<ExternalSession>>,
}

impl Listed {
    fn sessions(&self) -> Vec<ExternalSession> {
        self.by_agent.values().flatten().cloned().collect()
    }
}

/// Whether the programs are being asked for their sessions, and who waits for the answer.
#[derive(Debug)]
enum Asking<Waiter> {
    Not,
    Asked(Vec<Waiter>),
}

/// What the catalog's loop does next.
#[derive(Debug)]
enum Step<Waiter> {
    Answer(Waiter, Vec<ExternalSession>),
    /// Ask the programs for their sessions, and give the answer to [`Listing::listed`].
    List,
}

impl<Waiter> Listing<Waiter> {
    /// Takes a read: answered from the last listing if there is one, or at once with none for
    /// `cached_only`; else when the programs answer. A listing older than [`CATALOG_TTL`], or
    /// none, is asked again.
    fn asked(&mut self, waiter: Waiter, cached_only: bool, now: Instant) -> Vec<Step<Waiter>> {
        let mut steps = Vec::new();
        match &self.last {
            Some(last) => {
                steps.push(Step::Answer(waiter, last.sessions()));
                if now.saturating_duration_since(last.at) >= CATALOG_TTL {
                    self.ask(&mut steps);
                }
            }
            None if cached_only => {
                steps.push(Step::Answer(waiter, Vec::new()));
                self.ask(&mut steps);
            }
            None => {
                self.ask(&mut steps);
                if let Asking::Asked(waiters) = &mut self.asking {
                    waiters.push(waiter);
                }
            }
        }
        steps
    }

    /// Takes what the agents answered, and answers whoever waited for it. An agent whose
    /// sessions couldn't be listed keeps those it listed before, if any.
    fn listed(&mut self, told: Told, now: Instant) -> Vec<Step<Waiter>> {
        let mut by_agent = self.last.take().map_or_default(|last| last.by_agent);
        for (agent, outcome) in told {
            match outcome {
                Ok(sessions) => {
                    by_agent.insert(agent, sessions);
                }
                Err(error) => {
                    tracing::warn!(%agent, %error, "keeping the sessions it listed before");
                }
            }
        }
        let last = Listed { at: now, by_agent };
        let waiters = match std::mem::replace(&mut self.asking, Asking::Not) {
            Asking::Not => Vec::new(),
            Asking::Asked(waiters) => waiters,
        };
        let steps = waiters
            .into_iter()
            .map(|waiter| Step::Answer(waiter, last.sessions()))
            .collect();
        self.last = Some(last);
        steps
    }

    fn ask(&mut self, steps: &mut Vec<Step<Waiter>>) {
        if let Asking::Not = self.asking {
            self.asking = Asking::Asked(Vec::new());
            steps.push(Step::List);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ids::SessionId;
    use crate::provider::Backend;

    fn session(raw_id: &str) -> ExternalSession {
        ExternalSession {
            backend: Backend::Agy,
            raw_id: raw_id.to_owned(),
            id: SessionId::for_external(Backend::Agy, raw_id),
            cwd: PathBuf::from("/work"),
            title: raw_id.to_owned(),
            modified: None,
        }
    }

    #[derive(Debug)]
    enum Reader {
        A,
        B,
        C,
        D,
    }

    /// Writes the steps as "answer READER: RAW IDS" and "list".
    fn steps(steps: Vec<Step<Reader>>) -> Vec<String> {
        steps
            .into_iter()
            .map(|step| match step {
                Step::Answer(reader, sessions) => {
                    let ids: Vec<_> = sessions.iter().map(|it| it.raw_id.as_str()).collect();
                    format!("answer {reader:?}: {}", ids.join(" "))
                }
                Step::List => "list".to_owned(),
            })
            .collect()
    }

    /// agy listing the sessions `raw_ids`.
    fn agy(raw_ids: &[&str]) -> Told {
        vec![(
            Agent::Agy,
            Ok(raw_ids.iter().map(|id| session(id)).collect()),
        )]
    }

    const NOTHING: [&str; 0] = [];

    #[test]
    fn the_first_reads_wait_together_for_one_listing() {
        let start = Instant::now();
        let mut listing = Listing::default();
        assert_eq!(steps(listing.asked(Reader::A, false, start)), ["list"]);
        assert_eq!(steps(listing.asked(Reader::B, false, start)), NOTHING);
        assert_eq!(
            steps(listing.listed(agy(&["x"]), start)),
            ["answer A: x", "answer B: x"]
        );
    }

    #[test]
    fn a_read_of_the_cache_alone_is_answered_at_once() {
        let start = Instant::now();
        let mut listing = Listing::default();
        assert_eq!(
            steps(listing.asked(Reader::A, true, start)),
            ["answer A: ", "list"]
        );
        assert_eq!(steps(listing.asked(Reader::B, false, start)), NOTHING);
        assert_eq!(steps(listing.listed(agy(&["x"]), start)), ["answer B: x"]);
    }

    #[test]
    fn a_fresh_listing_answers_without_asking_again() {
        let start = Instant::now();
        let mut listing = Listing::default();
        listing.asked(Reader::A, false, start);
        listing.listed(agy(&["x"]), start);
        let later = start + CATALOG_TTL - Duration::from_secs(1);
        assert_eq!(
            steps(listing.asked(Reader::B, false, later)),
            ["answer B: x"]
        );
    }

    #[test]
    fn a_stale_listing_answers_and_is_asked_again_once() {
        let start = Instant::now();
        let mut listing = Listing::default();
        listing.asked(Reader::A, false, start);
        listing.listed(agy(&["x"]), start);
        let stale = start + CATALOG_TTL;
        assert_eq!(
            steps(listing.asked(Reader::B, false, stale)),
            ["answer B: x", "list"]
        );
        assert_eq!(
            steps(listing.asked(Reader::C, true, stale)),
            ["answer C: x"]
        );
        assert_eq!(steps(listing.listed(agy(&["y"]), stale)), NOTHING);
        assert_eq!(
            steps(listing.asked(Reader::D, false, stale)),
            ["answer D: y"]
        );
    }

    #[test]
    fn an_agent_that_fails_to_list_keeps_the_sessions_it_listed_before() {
        let start = Instant::now();
        let mut listing = Listing::default();
        listing.asked(Reader::A, false, start);
        listing.listed(
            vec![
                (Agent::Agy, Ok(vec![session("x")])),
                (Agent::Opencode, Ok(vec![session("o")])),
            ],
            start,
        );
        let stale = start + CATALOG_TTL;
        listing.asked(Reader::B, false, stale);
        listing.listed(
            vec![
                (Agent::Agy, Err(ListingError::TimedOut)),
                (Agent::Opencode, Ok(Vec::new())),
            ],
            stale,
        );
        assert_eq!(
            steps(listing.asked(Reader::C, false, stale)),
            ["answer C: x"]
        );
    }

    #[test]
    fn a_listing_that_tells_nothing_still_answers_who_waited() {
        let start = Instant::now();
        let mut listing = Listing::default();
        listing.asked(Reader::A, false, start);
        assert_eq!(steps(listing.listed(Vec::new(), start)), ["answer A: "]);
        assert_eq!(
            steps(listing.asked(Reader::B, false, start)),
            ["answer B: "]
        );
    }
}
