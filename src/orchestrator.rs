//! Multicommand orchestrator — concurrent task dispatch and signal handling.
//!
//! Spawns MCP tool calls via tokio::spawn, assigns PIDs, and maintains a signal log.
//! LLM wakeup is the client's responsibility; this module only coordinates tasks.

use crate::call;
use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;

const LOG_WINDOW: usize = 20;

/// Signal type for task lifecycle events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum SignalType {
    Init,
    Exit,
    Wait,
    Kill,
}

/// A single signal from a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSignal {
    pub pid: u64,
    #[serde(rename = "type")]
    pub signal_type: SignalType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TaskSignal {
    fn init(pid: u64, desc: &str) -> Self {
        Self {
            pid,
            signal_type: SignalType::Init,
            output: Some(desc.to_string()),
            error: None,
        }
    }
    fn exit_ok(pid: u64, output: String) -> Self {
        Self {
            pid,
            signal_type: SignalType::Exit,
            output: Some(output),
            error: None,
        }
    }
    fn exit_err(pid: u64, err: String) -> Self {
        Self {
            pid,
            signal_type: SignalType::Exit,
            output: None,
            error: Some(err),
        }
    }
    fn kill(pid: u64) -> Self {
        Self {
            pid,
            signal_type: SignalType::Kill,
            output: None,
            error: None,
        }
    }
}

/// One task in a dispatch batch.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DispatchTask {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// Dispatch request from the LLM.
#[derive(Debug, Deserialize)]
pub struct DispatchRequest {
    pub tasks: Vec<DispatchTask>,
}

struct OrchestratorState {
    /// PID -> JoinHandle for abort/kill
    handles: HashMap<u64, JoinHandle<()>>,
    /// Rolling log of last N signals
    log: Vec<TaskSignal>,
    /// Completed/failed since last get_task_status
    completed_since_last: Vec<TaskSignal>,
    next_pid: u64,
}

/// Orchestrator for concurrent MCP tool dispatch.
pub struct Orchestrator {
    paths: Arc<Paths>,
    sender: mpsc::Sender<TaskSignal>,
    state: Arc<RwLock<OrchestratorState>>,
}

impl Orchestrator {
    pub fn new(paths: Arc<Paths>) -> Self {
        let (tx, mut rx) = mpsc::channel::<TaskSignal>(64);
        let state = Arc::new(RwLock::new(OrchestratorState {
            handles: HashMap::new(),
            log: Vec::new(),
            completed_since_last: Vec::new(),
            next_pid: 1,
        }));

        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            while let Some(sig) = rx.recv().await {
                let pid = sig.pid;
                let is_complete =
                    sig.signal_type == SignalType::Exit || sig.signal_type == SignalType::Kill;
                let mut s = state_clone.write().await;
                s.log.push(sig.clone());
                if s.log.len() > LOG_WINDOW {
                    s.log.remove(0);
                }
                if is_complete {
                    s.completed_since_last.push(sig);
                }
                s.handles.remove(&pid);
            }
        });

        Self {
            paths,
            sender: tx,
            state,
        }
    }

    /// Dispatch tasks and return assigned PIDs.
    pub async fn dispatch_tasks(&self, req: DispatchRequest) -> Result<Vec<u64>, String> {
        let mut pids = Vec::with_capacity(req.tasks.len());
        let mut state = self.state.write().await;

        for task in req.tasks {
            let pid = state.next_pid;
            state.next_pid = pid.saturating_add(1);

            let paths = Arc::clone(&self.paths);
            let server = task.server.clone();
            let tool = task.tool.clone();
            let params = task.params.clone();
            let tx = self.sender.clone();

            let desc = format!("{} {}", server, tool);
            let _ = tx.send(TaskSignal::init(pid, &desc)).await;

            let handle = tokio::spawn(async move {
                let result = call::call_tool(&paths, &server, &tool, params).await;
                let sig = match result {
                    Ok(res) => {
                        let text = crate::call::format_call_result(&res);
                        TaskSignal::exit_ok(pid, text)
                    }
                    Err(e) => TaskSignal::exit_err(pid, e.to_string()),
                };
                let _ = tx.send(sig).await;
            });

            state.handles.insert(pid, handle);
            pids.push(pid);
        }

        Ok(pids)
    }

    /// Get completed/failed tasks since last call and optionally the rolling log.
    pub async fn get_task_status(&self, include_log: bool) -> serde_json::Value {
        let mut state = self.state.write().await;
        let completed = std::mem::take(&mut state.completed_since_last);
        let log = if include_log {
            state.log.clone()
        } else {
            vec![]
        };

        serde_json::json!({
            "completed": completed,
            "log": log,
        })
    }

    /// Kill a task by PID.
    pub async fn kill_task(&self, pid: u64) -> Result<bool, String> {
        let handle = {
            let mut state = self.state.write().await;
            state.handles.remove(&pid)
        };

        match handle {
            Some(h) => {
                h.abort();
                let _ = self.sender.send(TaskSignal::kill(pid)).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
