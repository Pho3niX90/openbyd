//! The other language's side of a client: where events go, and how it answers.
//!
//! The client reports to its [`Host`] with named events carrying a JSON
//! object. Three of them are questions (`gate_policy`, `pin_resolver`,
//! `relogin`): their payload has a `request` number, and the host answers by
//! calling the client's `resolve` with that number. A question is only asked
//! when the client's options say the host decides.
//!
//! Events arrive on the runtime's threads, in the order they happened. A
//! host must not block there: it queues the event for its own language's
//! thread and returns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Duration;

use openbyd_protocol::error::{Error, ErrorKind};
use serde_json::{Map, Value};
use tokio::sync::oneshot;

/// How long a worker waits for the answer to `gate_policy` or
/// `pin_resolver` before it takes the default.
pub const DECISION_TIMEOUT: Duration = Duration::from_secs(30);

/// What a client reports to: the binding of another language.
pub trait Host: Send + Sync + 'static {
    /// Delivers the event `name` with its payload, a JSON object as text.
    fn event(&self, name: &str, payload: &str);
}

/// A host that drops every event, for calls that need a client but no callbacks.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHost;

impl Host for NoHost {
    fn event(&self, _name: &str, _payload: &str) {}
}

type Answer = Result<Value, Error>;

enum Waiter {
    Blocking(mpsc::SyncSender<Answer>),
    Task(oneshot::Sender<Answer>),
}

/// The questions a client has asked its host and not yet had answered.
#[derive(Default)]
pub(crate) struct Requests {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Waiter>>,
}

impl Requests {
    fn pending(&self) -> MutexGuard<'_, HashMap<u64, Waiter>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, waiter: Waiter) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.pending().insert(id, waiter);
        id
    }

    /// Hands `answer` to whoever asked question `id`. `false` when nobody
    /// waits for it (answered already, timed out, or never asked).
    pub fn resolve(&self, id: u64, answer: Answer) -> bool {
        match self.pending().remove(&id) {
            Some(Waiter::Blocking(sender)) => sender.send(answer).is_ok(),
            Some(Waiter::Task(sender)) => sender.send(answer).is_ok(),
            None => false,
        }
    }

    /// How many questions are waiting for an answer.
    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.pending().len()
    }

    /// Asks `host` the question `name` and waits for the answer on this
    /// thread, at most `timeout`. `None` when the host answered with an
    /// error or not in time: the caller takes its default.
    ///
    /// On a worker of the multi-thread runtime the wait moves the worker's
    /// other tasks elsewhere.
    pub fn decide(
        &self,
        host: &dyn Host,
        name: &str,
        mut payload: Map<String, Value>,
        timeout: Duration,
    ) -> Option<Value> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let id = self.register(Waiter::Blocking(sender));
        payload.insert("request".into(), id.into());
        host.event(name, &Value::Object(payload).to_string());
        let wait = || receiver.recv_timeout(timeout);
        let answer = match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(wait)
            }
            _ => wait(),
        };
        self.pending().remove(&id);
        answer.ok().and_then(Result::ok)
    }

    /// Asks `host` the question `name` and awaits the answer without
    /// blocking a worker, however long it takes.
    pub async fn ask(
        &self,
        host: &dyn Host,
        name: &str,
        mut payload: Map<String, Value>,
    ) -> Answer {
        let (sender, receiver) = oneshot::channel();
        let id = self.register(Waiter::Task(sender));
        payload.insert("request".into(), id.into());
        host.event(name, &Value::Object(payload).to_string());
        receiver.await.unwrap_or_else(|_| {
            Err(Error::new(
                ErrorKind::BydError,
                format!("{name} was never answered"),
            ))
        })
    }

    /// Fails every waiting question: the client is closing.
    pub fn abandon(&self) {
        let waiting: Vec<Waiter> = self.pending().drain().map(|(_, waiter)| waiter).collect();
        for waiter in waiting {
            let gone = Err(Error::new(ErrorKind::BydError, "the client was closed"));
            match waiter {
                Waiter::Blocking(sender) => drop(sender.send(gone)),
                Waiter::Task(sender) => drop(sender.send(gone)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    /// Answers every question at once with a fixed answer, remembering the events.
    struct Echo {
        requests: Arc<Requests>,
        answer: Option<Answer>,
        seen: Mutex<Vec<(String, Value)>>,
    }

    impl Host for Echo {
        fn event(&self, name: &str, payload: &str) {
            let payload: Value = serde_json::from_str(payload).unwrap();
            let id = payload["request"].as_u64().unwrap();
            self.seen.lock().unwrap().push((name.to_owned(), payload));
            if let Some(answer) = &self.answer {
                self.requests.resolve(id, answer.clone());
            }
        }
    }

    fn echo(answer: Option<Answer>) -> (Arc<Requests>, Echo) {
        let requests = Arc::new(Requests::default());
        let host = Echo {
            requests: Arc::clone(&requests),
            answer,
            seen: Mutex::new(Vec::new()),
        };
        (requests, host)
    }

    fn fields(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_decision_returns_the_hosts_answer() {
        let (requests, host) = echo(Some(Ok(json!(true))));
        let answer = requests.decide(
            &host,
            "gate_policy",
            fields(json!({"vin": "V"})),
            DECISION_TIMEOUT,
        );
        assert_eq!(answer, Some(json!(true)));
        let seen = host.seen.lock().unwrap();
        assert_eq!(seen[0].0, "gate_policy");
        assert_eq!(seen[0].1["vin"], "V");
        assert_eq!(requests.waiting(), 0);
    }

    #[test]
    fn a_decision_without_an_answer_times_out_to_the_default() {
        let (requests, host) = echo(None);
        let answer = requests.decide(&host, "pin_resolver", Map::new(), Duration::from_millis(20));
        assert_eq!(answer, None);
        assert_eq!(requests.waiting(), 0);
        // A late answer finds nobody.
        assert!(!requests.resolve(1, Ok(json!("x"))));
    }

    #[test]
    fn a_decision_answered_with_an_error_takes_the_default() {
        let failed = Err(Error::new(ErrorKind::BydError, "the callback threw"));
        let (requests, host) = echo(Some(failed));
        assert_eq!(
            requests.decide(&host, "gate_policy", Map::new(), DECISION_TIMEOUT),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_decision_on_a_worker_does_not_stall_the_runtime() {
        let (requests, host) = echo(Some(Ok(json!("pin"))));
        let answer = tokio::spawn(async move {
            requests.decide(&host, "pin_resolver", Map::new(), DECISION_TIMEOUT)
        })
        .await
        .unwrap();
        assert_eq!(answer, Some(json!("pin")));
    }

    #[tokio::test]
    async fn a_question_awaits_its_answer_or_its_error() {
        let (requests, host) = echo(Some(Ok(json!({"user_id": "1"}))));
        let answer = requests.ask(&host, "relogin", Map::new()).await.unwrap();
        assert_eq!(answer["user_id"], "1");

        let refused = Err(Error::api(
            ErrorKind::BydAuthenticationError,
            "no",
            "1",
            "/e",
        ));
        let (requests, host) = echo(Some(refused));
        let error = requests
            .ask(&host, "relogin", Map::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BydAuthenticationError);
    }

    #[tokio::test]
    async fn closing_fails_the_questions_still_waiting() {
        let (requests, host) = echo(None);
        let asked = Arc::clone(&requests);
        let waiting = tokio::spawn(async move { asked.ask(&host, "relogin", Map::new()).await });
        while requests.waiting() == 0 {
            tokio::task::yield_now().await;
        }
        requests.abandon();
        let error = waiting.await.unwrap().unwrap_err();
        assert_eq!(error.message(), "the client was closed");
    }

    #[test]
    fn no_host_drops_events() {
        NoHost.event("anything", "{}");
    }
}
