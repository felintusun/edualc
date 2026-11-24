//! The sessions agy and opencode started elsewhere, listed once for the reads that ask at the
//! same time.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::acp::{self, Agent, ExternalSession};

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
    let (listed, mut listings) = mpsc::channel::<Vec<ExternalSession>>(4);
    loop {
        let steps = tokio::select! {
            query = queries.recv() => {
                let Some(CatalogQuery { cached_only, reply }) = query else {
                    return;
                };
                listing.asked(reply, cached_only, Instant::now())
            }
            Some(sessions) = listings.recv() => listing.listed(sessions, Instant::now()),
        };
        for step in steps {
            match step {
                Step::Answer(reply, sessions) => {
                    if reply.send(sessions).is_err() {
                        tracing::debug!("a read of the catalog went away");
                    }
                }
                Step::List => {
                    tokio::spawn(list_all(listed.clone()));
                }
            }
        }
    }
}

/// Asks agy and opencode for their sessions, and tells the catalog.
async fn list_all(listed: mpsc::Sender<Vec<ExternalSession>>) {
    let (agy, opencode) = tokio::join!(
        acp::list_sessions(Agent::Agy),
        acp::list_sessions(Agent::Opencode),
    );
    let mut sessions = agy;
    sessions.extend(opencode);
    if listed.send(sessions).await.is_err() {
        tracing::debug!("the catalog ended before its listing did");
    }
}

pub(super) async fn query_catalog(catalog: &Catalog, cached_only: bool) -> Vec<ExternalSession> {
    let (reply, answer) = oneshot::channel();
    if catalog
        .send(CatalogQuery { cached_only, reply })
        .await
        .is_err()
    {
        return Vec::new();
    }
    answer.await.unwrap_or_default()
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
    sessions: Vec<ExternalSession>,
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
                steps.push(Step::Answer(waiter, last.sessions.clone()));
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

    /// Takes the programs' answer, and answers whoever waited for it.
    fn listed(&mut self, sessions: Vec<ExternalSession>, now: Instant) -> Vec<Step<Waiter>> {
        let waiters = match std::mem::replace(&mut self.asking, Asking::Not) {
            Asking::Not => Vec::new(),
            Asking::Asked(waiters) => waiters,
        };
        let steps = waiters
            .into_iter()
            .map(|waiter| Step::Answer(waiter, sessions.clone()))
            .collect();
        self.last = Some(Listed { at: now, sessions });
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
    fn told(steps: Vec<Step<Reader>>) -> Vec<String> {
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

    const NOTHING: [&str; 0] = [];

    #[test]
    fn the_first_reads_wait_together_for_one_listing() {
        let start = Instant::now();
        let mut listing = Listing::default();
        assert_eq!(told(listing.asked(Reader::A, false, start)), ["list"]);
        assert_eq!(told(listing.asked(Reader::B, false, start)), NOTHING);
        assert_eq!(
            told(listing.listed(vec![session("x")], start)),
            ["answer A: x", "answer B: x"]
        );
    }

    #[test]
    fn a_read_of_the_cache_alone_is_answered_at_once() {
        let start = Instant::now();
        let mut listing = Listing::default();
        assert_eq!(
            told(listing.asked(Reader::A, true, start)),
            ["answer A: ", "list"]
        );
        assert_eq!(told(listing.asked(Reader::B, false, start)), NOTHING);
        assert_eq!(
            told(listing.listed(vec![session("x")], start)),
            ["answer B: x"]
        );
    }

    #[test]
    fn a_fresh_listing_answers_without_asking_again() {
        let start = Instant::now();
        let mut listing = Listing::default();
        listing.asked(Reader::A, false, start);
        listing.listed(vec![session("x")], start);
        let later = start + CATALOG_TTL - Duration::from_secs(1);
        assert_eq!(
            told(listing.asked(Reader::B, false, later)),
            ["answer B: x"]
        );
    }

    #[test]
    fn a_stale_listing_answers_and_is_asked_again_once() {
        let start = Instant::now();
        let mut listing = Listing::default();
        listing.asked(Reader::A, false, start);
        listing.listed(vec![session("x")], start);
        let stale = start + CATALOG_TTL;
        assert_eq!(
            told(listing.asked(Reader::B, false, stale)),
            ["answer B: x", "list"]
        );
        assert_eq!(told(listing.asked(Reader::C, true, stale)), ["answer C: x"]);
        assert_eq!(told(listing.listed(vec![session("y")], stale)), NOTHING);
        assert_eq!(
            told(listing.asked(Reader::D, false, stale)),
            ["answer D: y"]
        );
    }
}
