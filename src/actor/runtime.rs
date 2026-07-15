use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration as StdDuration;

use kameo::actor::{ActorRef, Spawn};
use kameo::message::{Context, Message};
use serde_json::Value;
use tokio::sync::broadcast;

use super::*;

const ACTOR_CONTINUATION_DELAY: StdDuration = StdDuration::from_secs(2);

#[derive(kameo::Actor)]
pub struct ActorSupervisor {
    pub(crate) registry: ActorRegistry,
    workers: HashMap<String, ActorRef<ResidentActor>>,
    executor: Option<ActorTurnExecutor>,
}

impl ActorSupervisor {
    pub(crate) fn new(registry: ActorRegistry) -> Self {
        Self {
            registry,
            workers: HashMap::new(),
            executor: None,
        }
    }

    pub(crate) fn sync_resident_actors(&mut self, supervisor_ref: ActorRef<ActorSupervisor>) {
        let active_ids = self
            .registry
            .actors
            .values()
            .filter(|actor| {
                !actor.is_principal
                    && matches!(actor.state, ActorState::Running | ActorState::Waiting)
            })
            .map(|actor| actor.id.clone())
            .collect::<std::collections::HashSet<_>>();
        self.workers
            .retain(|actor_id, worker| active_ids.contains(actor_id) && worker.is_alive());
        for actor_id in active_ids {
            self.workers.entry(actor_id.clone()).or_insert_with(|| {
                ResidentActor::spawn(ResidentActor {
                    actor_id: actor_id.clone(),
                    supervisor: supervisor_ref.clone(),
                })
            });
        }
    }

    pub(crate) fn wake_actor(&self, actor_id: &str, reason: impl Into<String>) -> bool {
        let Some(worker) = self.workers.get(actor_id) else {
            return false;
        };
        worker
            .tell(WakeActor {
                reason: reason.into(),
            })
            .try_send()
            .is_ok()
    }

    pub(crate) fn wake_active_actors(&self, reason: impl Into<String>) -> usize {
        let reason = reason.into();
        self.workers
            .keys()
            .filter(|actor_id| self.wake_actor(actor_id, reason.clone()))
            .count()
    }
}

#[derive(Clone, Debug)]
pub struct ActorRuntime {
    pub(crate) supervisor: ActorRef<ActorSupervisor>,
}

impl ActorRuntime {
    pub fn new(registry: ActorRegistry) -> Self {
        let supervisor = if tokio::runtime::Handle::try_current().is_ok() {
            ActorSupervisor::spawn(ActorSupervisor::new(registry))
        } else {
            let runtime = fallback_actor_runtime();
            let _guard = runtime.enter();
            ActorSupervisor::spawn(ActorSupervisor::new(registry))
        };
        let runtime = Self { supervisor };
        let _ = runtime.supervisor.tell(SyncResidentActors).try_send();
        runtime
    }

    pub fn install_turn_executor(&self, executor: ActorTurnExecutor) -> ActorResult<()> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return self
                .supervisor
                .tell(InstallTurnExecutor { executor })
                .try_send()
                .map_err(|_| ActorError::Runtime("actor runtime unavailable".to_string()));
        }
        self.supervisor
            .ask(InstallTurnExecutor { executor })
            .blocking_send()
            .map_err(actor_runtime_error)
    }

    pub async fn build_system_prompt(&self, actor_id: &str) -> ActorResult<String> {
        self.supervisor
            .ask(BuildSystemPrompt {
                actor_id: actor_id.to_string(),
            })
            .await
            .map_err(actor_runtime_error)
    }

    pub async fn build_requestable_directory(
        &self,
        actor_id: &str,
        has_agent_id_state: bool,
    ) -> ActorResult<String> {
        self.supervisor
            .ask(BuildRequestableDirectory {
                actor_id: actor_id.to_string(),
                has_agent_id_state,
            })
            .await
            .map_err(actor_runtime_error)
    }

    /// Stop the supervisor and every resident worker. A hosting process that
    /// drops agents while the process lives (e.g. an LRU agent cache) must
    /// call this: the supervisor and its workers hold mutually referencing
    /// `ActorRef`s, so dropping the last external handle alone would leave the
    /// kameo actors alive forever. Actor state is already persisted
    /// write-through, so a later rebuild restores unfinished subagents the
    /// same way a process restart does.
    pub fn shutdown(&self) {
        let _ = self.supervisor.tell(Shutdown).try_send();
    }

    pub async fn is_subagent(&self, actor_id: &str) -> bool {
        self.supervisor
            .ask(IsSubagent {
                actor_id: actor_id.to_string(),
            })
            .await
            .unwrap_or(false)
    }

    /// Sync entry point used by [`ToolRegistry::execute`] when invoked outside
    /// an async context (CLI subcommands that share the tool registry).
    pub fn execute_actor_tool_blocking(&self, command: ActorToolCommand) -> String {
        self.supervisor
            .ask(command)
            .blocking_send()
            .unwrap_or_else(|error| format!("Error: actor runtime unavailable: {error:?}"))
    }

    pub async fn execute_actor_tool(&self, command: ActorToolCommand) -> String {
        self.supervisor
            .ask(command)
            .await
            .unwrap_or_else(|error| format!("Error: actor runtime unavailable: {error:?}"))
    }

    /// Typed spawn helper for programmatic callers that need the new actor id
    /// without re-parsing the LLM-facing display message.
    pub async fn spawn_subagent(&self, request: SpawnSubagent) -> Result<SpawnReport, String> {
        self.supervisor
            .ask(request)
            .await
            .map_err(|error| format!("Error: actor runtime unavailable: {error:?}"))
    }

    pub async fn active_count(&self) -> usize {
        self.supervisor.ask(ActiveActorCount).await.unwrap_or(0)
    }

    pub async fn find_by_name(&self, name: &str, group: Option<&str>) -> Option<ActorInfo> {
        self.supervisor
            .ask(FindActorByName {
                name: name.to_string(),
                group: group.map(str::to_string),
            })
            .await
            .unwrap_or(None)
    }

    pub async fn actor_info(&self, actor_id: &str) -> Option<ActorInfo> {
        self.supervisor
            .ask(GetActorInfo {
                actor_id: actor_id.to_string(),
            })
            .await
            .unwrap_or(None)
    }

    pub async fn pop_inbox(&self, actor_id: &str) -> Option<ActorMessage> {
        self.supervisor
            .ask(PopActorInbox {
                actor_id: actor_id.to_string(),
            })
            .await
            .unwrap_or(None)
    }

    pub async fn task_state(&self, actor_id: &str) -> Option<TaskState> {
        self.supervisor
            .ask(GetActorTaskState {
                actor_id: actor_id.to_string(),
            })
            .await
            .unwrap_or(None)
    }

    pub async fn user_notification_events(
        &self,
        limit: usize,
    ) -> ActorResult<Vec<ActorNamedEvent>> {
        self.supervisor
            .ask(UserNotificationEvents { limit })
            .await
            .map_err(actor_runtime_error)
    }

    pub async fn principal_task_update_events(
        &self,
        principal_id: &str,
        limit: usize,
    ) -> ActorResult<Vec<ActorNamedEvent>> {
        self.supervisor
            .ask(PrincipalTaskUpdateEvents {
                principal_id: principal_id.to_string(),
                limit,
            })
            .await
            .map_err(actor_runtime_error)
    }

    /// Install a live broadcaster on the event bus and return a fresh
    /// subscriber. Subsequent calls swap the broadcaster, but historical
    /// receivers keep working until they're dropped.
    pub async fn install_event_broadcaster(
        &self,
        capacity: usize,
    ) -> ActorResult<broadcast::Receiver<ActorEvent>> {
        self.supervisor
            .ask(InstallEventBroadcaster { capacity })
            .await
            .map_err(actor_runtime_error)
    }

    pub async fn list_actors(&self) -> ActorResult<Vec<ActorInfo>> {
        self.supervisor
            .ask(ListAllActors)
            .await
            .map_err(actor_runtime_error)
    }

    /// Formatted lines describing unfinished subagent work (see
    /// [`ActorRegistry::open_work_lines`]). Used by the heartbeat digest.
    pub async fn open_work_lines(&self) -> ActorResult<Vec<String>> {
        self.supervisor
            .ask(OpenWorkLines)
            .await
            .map_err(actor_runtime_error)
    }
}

fn fallback_actor_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("lethe-actor-runtime")
            .build()
            .expect("failed to create fallback actor runtime")
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorNamedEvent {
    pub event: ActorEvent,
    pub actor_name: String,
}

#[derive(kameo::Actor)]
struct ResidentActor {
    actor_id: String,
    supervisor: ActorRef<ActorSupervisor>,
}

#[derive(Clone, Debug)]
struct WakeActor {
    reason: String,
}

impl Message<WakeActor> for ResidentActor {
    type Reply = ();

    async fn handle(
        &mut self,
        message: WakeActor,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let Some(executor) = self.supervisor.ask(GetTurnExecutor).await.ok().flatten() else {
            return;
        };
        let spec = match self
            .supervisor
            .ask(PrepareActorTurn {
                actor_id: self.actor_id.clone(),
            })
            .await
        {
            Ok(Some(spec)) => spec,
            Ok(None) | Err(_) => return,
        };

        let runtime = ActorRuntime {
            supervisor: self.supervisor.clone(),
        };
        let actor_id = spec.actor_id.clone();
        let outcome = executor(spec, runtime).await;
        let decision = self
            .supervisor
            .ask(CompleteActorTurn { actor_id, outcome })
            .await
            .ok();
        if let Some(decision) = decision
            && let Some(delay) = decision.continue_after
        {
            std::mem::drop(
                ctx.actor_ref()
                    .tell(WakeActor {
                        reason: format!("continue_after_{}", message.reason),
                    })
                    .send_after(delay),
            );
        }
    }
}

#[derive(Debug)]
struct ActorWakeDecision {
    continue_after: Option<StdDuration>,
}

#[derive(Debug)]
struct SyncResidentActors;

impl Message<SyncResidentActors> for ActorSupervisor {
    type Reply = ();

    async fn handle(
        &mut self,
        _message: SyncResidentActors,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.sync_resident_actors(ctx.actor_ref().clone());
    }
}

struct InstallTurnExecutor {
    executor: ActorTurnExecutor,
}

impl Message<InstallTurnExecutor> for ActorSupervisor {
    type Reply = ActorResult<()>;

    async fn handle(
        &mut self,
        message: InstallTurnExecutor,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.executor = Some(message.executor);
        self.sync_resident_actors(ctx.actor_ref().clone());
        self.wake_active_actors("executor_installed");
        Ok(())
    }
}

#[derive(Debug)]
struct GetTurnExecutor;

impl Message<GetTurnExecutor> for ActorSupervisor {
    type Reply = Option<ActorTurnExecutor>;

    async fn handle(
        &mut self,
        _message: GetTurnExecutor,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.executor.clone()
    }
}

#[derive(Debug)]
struct PrepareActorTurn {
    actor_id: String,
}

impl Message<PrepareActorTurn> for ActorSupervisor {
    type Reply = ActorResult<Option<ActorRunSpec>>;

    async fn handle(
        &mut self,
        message: PrepareActorTurn,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry.prepare_actor_turn(&message.actor_id)
    }
}

struct CompleteActorTurn {
    actor_id: String,
    outcome: ActorResult<String>,
}

impl Message<CompleteActorTurn> for ActorSupervisor {
    type Reply = ActorResult<ActorWakeDecision>;

    async fn handle(
        &mut self,
        message: CompleteActorTurn,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let response = match message.outcome {
            Ok(response) => response,
            Err(error) => format!("Runner error: {error}"),
        };
        self.registry
            .record_actor_turn_response(&message.actor_id, response)?;
        self.sync_resident_actors(ctx.actor_ref().clone());
        let continue_after = self
            .registry
            .should_autocontinue_actor(&message.actor_id)
            .then_some(ACTOR_CONTINUATION_DELAY);
        Ok(ActorWakeDecision { continue_after })
    }
}

#[derive(Debug)]
struct BuildSystemPrompt {
    actor_id: String,
}

impl Message<BuildSystemPrompt> for ActorSupervisor {
    type Reply = ActorResult<String>;

    async fn handle(
        &mut self,
        message: BuildSystemPrompt,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry.build_system_prompt(&message.actor_id)
    }
}

#[derive(Debug)]
struct BuildRequestableDirectory {
    actor_id: String,
    has_agent_id_state: bool,
}

impl Message<BuildRequestableDirectory> for ActorSupervisor {
    type Reply = ActorResult<String>;

    async fn handle(
        &mut self,
        message: BuildRequestableDirectory,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry
            .build_requestable_directory(&message.actor_id, message.has_agent_id_state)
    }
}

#[derive(Debug)]
struct Shutdown;

impl Message<Shutdown> for ActorSupervisor {
    type Reply = ();

    async fn handle(
        &mut self,
        _message: Shutdown,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Kill (not stop_gracefully): a worker mid-turn is awaiting an LLM
        // call inside its message handler; graceful stop would wait for it.
        // The registry's write-through store already holds the last completed
        // checkpoint, which is exactly what a process restart would resume
        // from.
        for worker in self.workers.values() {
            worker.kill();
        }
        self.workers.clear();
        ctx.actor_ref().kill();
    }
}

#[derive(Debug)]
struct IsSubagent {
    actor_id: String,
}

impl Message<IsSubagent> for ActorSupervisor {
    type Reply = bool;

    async fn handle(
        &mut self,
        message: IsSubagent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry
            .get(&message.actor_id)
            .map(|actor| !actor.is_principal)
            .unwrap_or(false)
    }
}

#[derive(Debug)]
struct ActiveActorCount;

impl Message<ActiveActorCount> for ActorSupervisor {
    type Reply = usize;

    async fn handle(
        &mut self,
        _message: ActiveActorCount,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry.active_count()
    }
}

#[derive(Debug)]
struct FindActorByName {
    name: String,
    group: Option<String>,
}

impl Message<FindActorByName> for ActorSupervisor {
    type Reply = Option<ActorInfo>;

    async fn handle(
        &mut self,
        message: FindActorByName,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry
            .find_by_name(&message.name, message.group.as_deref())
            .map(Actor::info)
    }
}

#[derive(Debug)]
struct GetActorInfo {
    actor_id: String,
}

impl Message<GetActorInfo> for ActorSupervisor {
    type Reply = Option<ActorInfo>;

    async fn handle(
        &mut self,
        message: GetActorInfo,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry.get(&message.actor_id).map(Actor::info)
    }
}

#[derive(Debug)]
struct PopActorInbox {
    actor_id: String,
}

impl Message<PopActorInbox> for ActorSupervisor {
    type Reply = Option<ActorMessage>;

    async fn handle(
        &mut self,
        message: PopActorInbox,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry.pop_inbox(&message.actor_id)
    }
}

#[derive(Debug)]
struct GetActorTaskState {
    actor_id: String,
}

impl Message<GetActorTaskState> for ActorSupervisor {
    type Reply = Option<TaskState>;

    async fn handle(
        &mut self,
        message: GetActorTaskState,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registry
            .get(&message.actor_id)
            .map(|actor| actor.task_state)
    }
}

#[derive(Clone, Debug)]
pub enum ActorToolCommand {
    SendMessage {
        actor_id: String,
        target_id: String,
        content: String,
        reply_to: Option<String>,
        channel: String,
        kind: String,
    },
    WaitForResponse {
        actor_id: String,
    },
    DiscoverActors {
        actor_id: String,
        group: Option<String>,
        include_terminated: bool,
    },
    SpawnActor {
        actor_id: String,
        name: String,
        goals: String,
        group: Option<String>,
        tools: String,
        model: String,
        max_turns: usize,
    },
    PingActor {
        actor_id: String,
        target_id: String,
    },
    KillActor {
        actor_id: String,
        target_id: String,
    },
    UpdateTaskState {
        actor_id: String,
        state: String,
        note: String,
    },
    GetTaskState {
        actor_id: String,
    },
    Terminate {
        actor_id: String,
        result: String,
        outcome: String,
        files_touched: String,
        follow_up: String,
    },
    RestartSelf {
        actor_id: String,
        new_goals: String,
    },
}

impl Message<ActorToolCommand> for ActorSupervisor {
    type Reply = String;

    async fn handle(
        &mut self,
        command: ActorToolCommand,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match command {
            ActorToolCommand::SendMessage {
                actor_id,
                target_id,
                content,
                reply_to,
                channel,
                kind,
            } => {
                let result = self
                    .registry
                    .send_message_tool(&actor_id, &target_id, &content, reply_to, &channel, &kind);
                self.sync_resident_actors(ctx.actor_ref().clone());
                self.wake_actor(&target_id, "message_received");
                result
            }
            ActorToolCommand::WaitForResponse { actor_id } => {
                match self.registry.pop_inbox(&actor_id) {
                    Some(message) => {
                        let sender_name = self
                            .registry
                            .get(&message.sender)
                            .map(|actor| actor.config.name.clone())
                            .unwrap_or_else(|| message.sender.clone());
                        format!("[From {sender_name}] {}", message.content)
                    }
                    None => "Timed out waiting for response.".to_string(),
                }
            }
            ActorToolCommand::DiscoverActors {
                actor_id,
                group,
                include_terminated,
            } => self
                .registry
                .discover_for_actor(&actor_id, group.as_deref(), include_terminated)
                .unwrap_or_else(|error| format!("Error: {error}")),
            ActorToolCommand::SpawnActor {
                actor_id,
                name,
                goals,
                group,
                tools,
                model,
                max_turns,
            } => {
                let outcome = self
                    .registry
                    .spawn_child_for_actor(
                        &actor_id,
                        ActorSpawnRequest {
                            name: &name,
                            goals: &goals,
                            group: group.as_deref(),
                            tools: &tools,
                            model: &model,
                            max_turns,
                        },
                    )
                    .map(|report| report.message().to_string())
                    .unwrap_or_else(|error| format!("Error: {error}"));
                self.sync_resident_actors(ctx.actor_ref().clone());
                self.wake_active_actors("actor_spawned");
                outcome
            }
            ActorToolCommand::PingActor {
                actor_id: _,
                target_id,
            } => self.registry.ping_actor(&target_id),
            ActorToolCommand::KillActor {
                actor_id,
                target_id,
            } => {
                let result = self.registry.kill_actor_tool(&actor_id, &target_id);
                self.sync_resident_actors(ctx.actor_ref().clone());
                result
            }
            ActorToolCommand::UpdateTaskState {
                actor_id,
                state,
                note,
            } => match self.registry.set_task_state(&actor_id, &state, note) {
                Ok(message) => message,
                Err(error) => format!("Error: {error}"),
            },
            ActorToolCommand::GetTaskState { actor_id } => self
                .registry
                .get(&actor_id)
                .map(|actor| {
                    let state = serde_json::to_string(&actor.task_state)
                        .unwrap_or_else(|_| "\"unknown\"".to_string())
                        .trim_matches('"')
                        .to_string();
                    format!("Task state: {state}")
                })
                .unwrap_or_else(|| format!("Actor {actor_id} not found.")),
            ActorToolCommand::Terminate {
                actor_id,
                result,
                outcome,
                files_touched,
                follow_up,
            } => {
                let result = self.registry.terminate_tool(
                    &actor_id,
                    &result,
                    &outcome,
                    &files_touched,
                    &follow_up,
                );
                self.sync_resident_actors(ctx.actor_ref().clone());
                result
            }
            ActorToolCommand::RestartSelf {
                actor_id,
                new_goals,
            } => {
                let result = self.registry.restart_self(&actor_id, &new_goals);
                self.sync_resident_actors(ctx.actor_ref().clone());
                result
            }
        }
    }
}

/// Typed spawn request used by programmatic callers (e.g. `spawn_chain`) that
/// need the new actor id back. The LLM-facing `ActorToolCommand::SpawnActor`
/// path goes through the same registry routine but renders the result to text.
#[derive(Debug)]
pub struct SpawnSubagent {
    pub actor_id: String,
    pub name: String,
    pub goals: String,
    pub group: Option<String>,
    pub tools: String,
    pub model: String,
    pub max_turns: usize,
}

impl Message<SpawnSubagent> for ActorSupervisor {
    type Reply = ActorResult<SpawnReport>;

    async fn handle(
        &mut self,
        message: SpawnSubagent,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let report = self.registry.spawn_child_for_actor(
            &message.actor_id,
            ActorSpawnRequest {
                name: &message.name,
                goals: &message.goals,
                group: message.group.as_deref(),
                tools: &message.tools,
                model: &message.model,
                max_turns: message.max_turns,
            },
        )?;
        self.sync_resident_actors(ctx.actor_ref().clone());
        self.wake_active_actors("actor_spawned");
        Ok(report)
    }
}

#[derive(Debug)]
struct UserNotificationEvents {
    limit: usize,
}

impl Message<UserNotificationEvents> for ActorSupervisor {
    type Reply = ActorResult<Vec<ActorNamedEvent>>;

    async fn handle(
        &mut self,
        message: UserNotificationEvents,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut events =
            self.registry
                .events
                .query(Some("user_notify"), None, None, message.limit.max(1));
        events.reverse();
        Ok(events
            .into_iter()
            .map(|event| {
                let actor_name = self
                    .registry
                    .get(&event.actor_id)
                    .map(|actor| actor.config.name.clone())
                    .unwrap_or_else(|| event.actor_id.clone());
                ActorNamedEvent { event, actor_name }
            })
            .collect())
    }
}

#[derive(Debug)]
struct PrincipalTaskUpdateEvents {
    principal_id: String,
    limit: usize,
}

impl Message<PrincipalTaskUpdateEvents> for ActorSupervisor {
    type Reply = ActorResult<Vec<ActorNamedEvent>>;

    async fn handle(
        &mut self,
        message: PrincipalTaskUpdateEvents,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let query_limit = message.limit.max(1).saturating_mul(8).max(64);
        let mut events = self
            .registry
            .events
            .query(Some("actor_message"), None, None, query_limit);
        events.reverse();
        let registry = &self.registry;
        Ok(events
            .into_iter()
            .filter_map(|event| {
                let recipient = event
                    .payload
                    .get("recipient")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if recipient != message.principal_id {
                    return None;
                }
                let channel = event
                    .payload
                    .get("channel")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let kind = event
                    .payload
                    .get("intent")
                    .or_else(|| event.payload.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if channel != "task_update"
                    || !MessageIntent::from_strings(channel, kind).wakes_cortex()
                {
                    return None;
                }
                let actor_name = registry
                    .get(&event.actor_id)
                    .map(|actor| actor.config.name.clone())
                    .unwrap_or_else(|| event.actor_id.clone());
                // DMN reflection is internal — its task_update progress
                // messages would otherwise wake cortex via the
                // actor-update-monitor → cortex chat_once → Telegram path,
                // and cortex parrots the verbose reflection back to the
                // user. DMN's legitimate user-facing signals go through
                // user_notify (gated by review_notifications_with_llm),
                // not through task_update.
                if actor_name == crate::actor::background::DMN_ACTOR_NAME {
                    return None;
                }
                Some(ActorNamedEvent { event, actor_name })
            })
            .take(message.limit.max(1))
            .collect())
    }
}

fn actor_runtime_error<M>(error: kameo::error::SendError<M, ActorError>) -> ActorError {
    ActorError::Runtime(format!("{error:?}"))
}

#[derive(Debug)]
struct InstallEventBroadcaster {
    capacity: usize,
}

impl Message<InstallEventBroadcaster> for ActorSupervisor {
    type Reply = ActorResult<broadcast::Receiver<ActorEvent>>;

    async fn handle(
        &mut self,
        message: InstallEventBroadcaster,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let (tx, rx) = broadcast::channel(message.capacity.max(16));
        self.registry.events.set_broadcaster(tx);
        Ok(rx)
    }
}

#[derive(Debug)]
struct OpenWorkLines;

impl Message<OpenWorkLines> for ActorSupervisor {
    type Reply = ActorResult<Vec<String>>;

    async fn handle(
        &mut self,
        _message: OpenWorkLines,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.registry.open_work_lines())
    }
}

#[derive(Debug)]
struct ListAllActors;

impl Message<ListAllActors> for ActorSupervisor {
    type Reply = ActorResult<Vec<ActorInfo>>;

    async fn handle(
        &mut self,
        _message: ListAllActors,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.registry.all_actors())
    }
}
