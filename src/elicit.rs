//! Inbound elicitation: a server asking for input *during* a tool call.
//!
//! Some operations cannot be made non-interactive and cannot be answered up
//! front, because they ask a sequence of questions that only appears as the work
//! unfolds — `fdisk`, `mysql_secure_installation`, a REPL, a vendor installer
//! with no `-y`. The non-interactive default (`--noconfirm`, `-y`) handles the
//! single-prompt case and cannot get past step 1 of a wizard.
//!
//! MCP already carries the answer: `elicitation/create` lets a server request
//! input mid-tool-call. The call does **not** return, so the process stays alive
//! and there is nothing to reattach to. This module is dmcp's client side of it.
//!
//! Two rules shape everything here:
//!
//! **Only advertise it where it can be answered.** Capability negotiation is a
//! promise. The one-shot `dmcp call` path has nobody to ask — it is a subprocess
//! that spawns a server, calls once, and exits — so it does not declare the
//! capability and declines anything that arrives regardless. Only a driver that
//! can actually reach a human (the session broker, which keeps the server alive
//! across the exchange) attaches a sink and declares support.
//!
//! **The prompt text is untrusted.** It is written by whoever wrote the server,
//! which for a community server is not the user and not us. It travels labeled
//! so a renderer attributes it to the server rather than presenting it as JARVIS
//! asking — the same provenance requirement the boundary protocol imposes
//! elsewhere. A server that asks for a password is phishing, or is a tool that
//! genuinely needs one; either way that decision belongs to a human upstream,
//! never to a model, so nothing here ever invents an answer.

use std::sync::atomic::{AtomicUsize, Ordering};

use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateElicitationRequestParams, CreateElicitationResult,
    ElicitationCapability, FormElicitationCapability,
};
use rmcp::service::RequestContext;
use rmcp::{ClientHandler, RoleClient};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// How many prompts one server may raise before the driver stops relaying them.
///
/// An interactive wizard asks a handful of questions; a server looping on
/// elicitation would otherwise hold a session — and the human's attention —
/// hostage indefinitely. Past the budget the client declines locally without
/// bothering anyone, which a server must already handle (declining is a normal
/// protocol outcome), so the tool call still terminates.
pub const DEFAULT_PROMPT_BUDGET: usize = 16;

fn prompt_budget() -> usize {
    std::env::var("DMCP_ELICIT_MAX_PROMPTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PROMPT_BUDGET)
}

/// What a server is asking for, in the shape a driver needs to render it.
///
/// `message` (and any `url`) is the **server's** text, not ours — see the module
/// note on provenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    /// The server that asked, so the prompt can be attributed.
    pub server: String,
    /// The server's own wording of the question. Untrusted.
    pub message: String,
    /// Form mode: the JSON Schema of the expected reply, when one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// URL mode: the page the server wants visited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// How a driver answered a [`PromptRequest`].
///
/// Mirrors MCP's own three outcomes so a driver cannot express anything the
/// protocol cannot carry: supply content, refuse this one question but let the
/// operation continue, or abandon the operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum PromptAnswer {
    /// The answer, matching the requested schema.
    Accept { content: serde_json::Value },
    /// No answer, but the server may carry on.
    Decline,
    /// Abandon the whole operation.
    Cancel,
}

impl From<PromptAnswer> for CreateElicitationResult {
    fn from(a: PromptAnswer) -> Self {
        use rmcp::model::ElicitationAction;
        match a {
            PromptAnswer::Accept { content } => CreateElicitationResult {
                action: ElicitationAction::Accept,
                // The reply must conform to the requested schema, which is an
                // object. Anything else is the driver's bug, and dropping it
                // beats sending the server a shape it cannot read.
                content: content.is_object().then_some(content),
            },
            PromptAnswer::Decline => CreateElicitationResult {
                action: ElicitationAction::Decline,
                content: None,
            },
            PromptAnswer::Cancel => CreateElicitationResult {
                action: ElicitationAction::Cancel,
                content: None,
            },
        }
    }
}

/// A parked prompt: what was asked, and where the answer goes.
pub struct Prompt {
    pub request: PromptRequest,
    pub answer: oneshot::Sender<PromptAnswer>,
}

/// Decline without asking anyone — the answer whenever there is no path to a
/// human, or the budget is spent.
fn declined() -> CreateElicitationResult {
    PromptAnswer::Decline.into()
}

/// dmcp's MCP client handler.
///
/// `unattended` is today's behavior made explicit: no capability advertised, and
/// any elicitation that arrives anyway is declined. `attended` routes prompts to
/// a driver that can reach a human.
#[derive(Clone)]
pub struct ServerClient {
    server_id: String,
    prompts: Option<mpsc::Sender<Prompt>>,
    used: std::sync::Arc<AtomicUsize>,
    budget: usize,
}

impl ServerClient {
    /// A client with nobody to ask: the one-shot `dmcp call` path.
    pub fn unattended(server_id: impl Into<String>) -> Self {
        Self {
            server_id: server_id.into(),
            prompts: None,
            used: Default::default(),
            budget: 0,
        }
    }

    /// A client whose prompts reach `sink`, which is expected to answer each
    /// one. Dropping the sender is itself an answer — a decline — so a driver
    /// that goes away cannot wedge a tool call.
    pub fn attended(server_id: impl Into<String>, sink: mpsc::Sender<Prompt>) -> Self {
        Self {
            server_id: server_id.into(),
            prompts: Some(sink),
            used: Default::default(),
            budget: prompt_budget(),
        }
    }

    fn can_ask(&self) -> bool {
        self.prompts.is_some()
    }

    /// Take one unit of budget, or report that it is spent.
    fn claim_budget(&self) -> bool {
        // fetch_update rather than load-then-store: two prompts racing in from
        // one server must not both pass on the last unit.
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.budget).then_some(n + 1)
            })
            .is_ok()
    }
}

/// Flatten rmcp's request enum into the shape a driver renders.
pub fn to_prompt_request(server: &str, params: CreateElicitationRequestParams) -> PromptRequest {
    match params {
        CreateElicitationRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } => PromptRequest {
            server: server.to_string(),
            message,
            schema: serde_json::to_value(requested_schema).ok(),
            url: None,
        },
        CreateElicitationRequestParams::UrlElicitationParams { message, url, .. } => {
            PromptRequest {
                server: server.to_string(),
                message,
                schema: None,
                url: Some(url),
            }
        }
    }
}

impl ClientHandler for ServerClient {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        // Advertise elicitation only when a prompt can actually be answered.
        // A server checks this before asking, so claiming it unconditionally
        // would invite questions into a path whose only reply is "no".
        if self.can_ask() {
            info.capabilities = ClientCapabilities {
                elicitation: Some(ElicitationCapability {
                    // Form mode only: URL mode asks the client to open a
                    // browser, which a headless daemon cannot promise.
                    form: Some(FormElicitationCapability::default()),
                    url: None,
                }),
                ..info.capabilities
            };
        }
        info
    }

    async fn create_elicitation(
        &self,
        request: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, rmcp::ErrorData> {
        Ok(self.ask(request).await)
    }
}

impl ServerClient {
    /// The whole elicitation policy, free of `RequestContext` so it can be
    /// exercised without a live peer on the other end of a socket.
    ///
    /// Every path ends in an answer. There is no branch that leaves the server
    /// waiting, because a wedged elicitation is a wedged tool call, which is the
    /// hang this whole mechanism exists to avoid.
    async fn ask(&self, request: CreateElicitationRequestParams) -> CreateElicitationResult {
        let Some(sink) = self.prompts.as_ref() else {
            return declined();
        };
        if !self.claim_budget() {
            return declined();
        }

        let (tx, rx) = oneshot::channel();
        let prompt = Prompt {
            request: to_prompt_request(&self.server_id, request),
            answer: tx,
        };
        if sink.send(prompt).await.is_err() {
            // The driver is gone. Decline so the server unblocks and the call
            // can finish, rather than leaving it waiting on nobody.
            return declined();
        }
        // A dropped answer channel resolves to Err here — same story: nothing is
        // coming, so decline rather than hang.
        rx.await.map(Into::into).unwrap_or_else(|_| declined())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ElicitationAction;

    fn form(message: &str) -> CreateElicitationRequestParams {
        CreateElicitationRequestParams::FormElicitationParams {
            meta: None,
            message: message.to_string(),
            requested_schema: rmcp::model::ElicitationSchema::new(Default::default()),
        }
    }

    /// The one-shot path promises nothing: a server checking capabilities sees
    /// no elicitation support, so it never asks a question only "no" can answer.
    #[test]
    fn an_unattended_client_does_not_advertise_elicitation() {
        let c = ServerClient::unattended("srv");
        assert!(c.get_info().capabilities.elicitation.is_none());
    }

    /// A driver that can reach a human advertises it — form mode only, since
    /// URL mode would promise a browser a daemon may not have.
    #[test]
    fn an_attended_client_advertises_form_elicitation_only() {
        let (tx, _rx) = mpsc::channel(1);
        let c = ServerClient::attended("srv", tx);
        let caps = c.get_info().capabilities.elicitation.expect("advertised");
        assert!(caps.form.is_some());
        assert!(
            caps.url.is_none(),
            "URL mode needs a browser we cannot promise"
        );
    }

    /// The prompt carries the server's identity, so a renderer can attribute the
    /// question instead of speaking it in JARVIS's voice.
    #[test]
    fn a_prompt_names_the_server_that_asked() {
        let req = to_prompt_request("com.example.installer", form("Erase disk? [y/N]"));
        assert_eq!(req.server, "com.example.installer");
        assert_eq!(req.message, "Erase disk? [y/N]");
        assert!(req.url.is_none());
    }

    /// URL-mode prompts survive the flattening too, so a driver can refuse or
    /// render them knowingly rather than seeing an empty question.
    #[test]
    fn a_url_prompt_keeps_its_url() {
        let params = CreateElicitationRequestParams::UrlElicitationParams {
            meta: None,
            message: "Authorize here".into(),
            url: "https://example.invalid/auth".into(),
            elicitation_id: "e1".into(),
        };
        let req = to_prompt_request("srv", params);
        assert_eq!(req.url.as_deref(), Some("https://example.invalid/auth"));
    }

    /// The three driver outcomes map onto MCP's own three, and only `accept`
    /// ever carries content.
    #[test]
    fn answers_map_onto_the_protocols_three_outcomes() {
        let accept: CreateElicitationResult = PromptAnswer::Accept {
            content: serde_json::json!({"answer": "yes"}),
        }
        .into();
        assert_eq!(accept.action, ElicitationAction::Accept);
        assert_eq!(accept.content.unwrap()["answer"], "yes");

        let decline: CreateElicitationResult = PromptAnswer::Decline.into();
        assert_eq!(decline.action, ElicitationAction::Decline);
        assert!(decline.content.is_none());

        let cancel: CreateElicitationResult = PromptAnswer::Cancel.into();
        assert_eq!(cancel.action, ElicitationAction::Cancel);
        assert!(cancel.content.is_none());
    }

    /// A non-object `accept` payload is downgraded rather than sent: the
    /// protocol carries an object, and a server cannot read anything else.
    #[test]
    fn a_malformed_accept_payload_does_not_reach_the_server() {
        let r: CreateElicitationResult = PromptAnswer::Accept {
            content: serde_json::json!("not an object"),
        }
        .into();
        assert!(r.content.is_none());
    }

    /// The budget is per-client and consumed exactly once per prompt, so a
    /// server cannot loop forever on questions.
    #[test]
    fn the_prompt_budget_is_finite_and_consumed_once_each() {
        let (tx, _rx) = mpsc::channel(1);
        let mut c = ServerClient::attended("srv", tx);
        c.budget = 2;
        assert!(c.claim_budget());
        assert!(c.claim_budget());
        assert!(!c.claim_budget(), "budget must not exceed its limit");
    }

    /// An unattended client has no budget at all — it never asks anyone.
    #[test]
    fn an_unattended_client_has_no_budget() {
        assert!(!ServerClient::unattended("srv").claim_budget());
    }

    /// With no driver attached, an elicitation that arrives anyway is declined
    /// — never left hanging, and never answered by us.
    #[tokio::test]
    async fn an_unattended_client_declines_rather_than_hangs() {
        let r = ServerClient::unattended("srv").ask(form("Proceed?")).await;
        assert_eq!(r.action, ElicitationAction::Decline);
    }

    /// A driver that disappears mid-exchange must not wedge the tool call: the
    /// dropped answer channel resolves to a decline so the server unblocks.
    #[tokio::test]
    async fn a_vanished_driver_becomes_a_decline() {
        let (tx, mut rx) = mpsc::channel(1);
        let c = ServerClient::attended("srv", tx);
        tokio::spawn(async move {
            // Take the prompt and drop its answer channel without replying.
            let _ = rx.recv().await;
        });
        let r = c.ask(form("Proceed?")).await;
        assert_eq!(r.action, ElicitationAction::Decline);
    }

    /// The happy path: the driver's answer reaches the server verbatim.
    #[tokio::test]
    async fn an_answered_prompt_returns_the_drivers_content() {
        let (tx, mut rx) = mpsc::channel(1);
        let c = ServerClient::attended("srv", tx);
        tokio::spawn(async move {
            let prompt = rx.recv().await.expect("prompt");
            assert_eq!(prompt.request.message, "Partition type?");
            let _ = prompt.answer.send(PromptAnswer::Accept {
                content: serde_json::json!({"type": "primary"}),
            });
        });
        let r = c.ask(form("Partition type?")).await;
        assert_eq!(r.action, ElicitationAction::Accept);
        assert_eq!(r.content.unwrap()["type"], "primary");
    }

    /// Past the budget the client stops relaying and answers locally, so a
    /// looping server cannot hold the session or the human hostage.
    #[tokio::test]
    async fn a_looping_server_is_cut_off_at_the_budget() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut c = ServerClient::attended("srv", tx);
        c.budget = 1;
        let relayed = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = relayed.clone();
        tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                seen.fetch_add(1, Ordering::SeqCst);
                let _ = p.answer.send(PromptAnswer::Decline);
            }
        });
        for _ in 0..5 {
            let r = c.ask(form("again?")).await;
            assert_eq!(r.action, ElicitationAction::Decline);
        }
        assert_eq!(
            relayed.load(Ordering::SeqCst),
            1,
            "only the budgeted prompt should ever reach the driver"
        );
    }
}
