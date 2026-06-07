use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use tokio::task::AbortHandle;

use crate::llm::LlmAttachment;

pub const DEFAULT_DEBOUNCE_SECONDS: f64 = 5.0;

pub type ConversationFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
pub type ProcessCallback =
    Arc<dyn Fn(ProcessContext) -> ConversationFuture + Send + Sync + 'static>;
type TaskFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingMessage {
    pub content: String,
    pub metadata: serde_json::Map<String, Value>,
    #[serde(default)]
    pub attachments: Vec<LlmAttachment>,
    pub created_at: DateTime<Utc>,
}

impl PendingMessage {
    pub fn new(
        content: impl Into<String>,
        metadata: Option<serde_json::Map<String, Value>>,
    ) -> Self {
        Self::new_with_attachments(content, metadata, Vec::new())
    }

    pub fn new_with_attachments(
        content: impl Into<String>,
        metadata: Option<serde_json::Map<String, Value>>,
        attachments: Vec<LlmAttachment>,
    ) -> Self {
        Self {
            content: content.into(),
            metadata: metadata.unwrap_or_default(),
            attachments,
            created_at: Utc::now(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct InterruptToken {
    flag: Arc<AtomicBool>,
}

impl InterruptToken {
    pub fn is_interrupted(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone, Debug)]
pub struct ProcessContext {
    pub chat_id: i64,
    pub user_id: i64,
    pub message: String,
    pub metadata: serde_json::Map<String, Value>,
    pub attachments: Vec<LlmAttachment>,
    pub interrupt: InterruptToken,
}

#[derive(Debug)]
pub struct ConversationState {
    pub chat_id: i64,
    pub user_id: i64,
    pending_messages: VecDeque<PendingMessage>,
    is_processing: bool,
    is_debouncing: bool,
    interrupt_requested: bool,
    current_interrupt: Option<InterruptToken>,
    debounce_notify: Arc<Notify>,
    current_task: Option<AbortHandle>,
    debounce_task: Option<AbortHandle>,
}

impl ConversationState {
    pub fn new(chat_id: i64, user_id: i64) -> Self {
        Self {
            chat_id,
            user_id,
            pending_messages: VecDeque::new(),
            is_processing: false,
            is_debouncing: false,
            interrupt_requested: false,
            current_interrupt: None,
            debounce_notify: Arc::new(Notify::new()),
            current_task: None,
            debounce_task: None,
        }
    }

    pub fn add_message(
        &mut self,
        content: impl Into<String>,
        metadata: Option<serde_json::Map<String, Value>>,
    ) -> (bool, bool) {
        self.add_message_with_attachments(content, metadata, Vec::new())
    }

    pub fn add_message_with_attachments(
        &mut self,
        content: impl Into<String>,
        metadata: Option<serde_json::Map<String, Value>>,
        attachments: Vec<LlmAttachment>,
    ) -> (bool, bool) {
        self.pending_messages
            .push_back(PendingMessage::new_with_attachments(
                content,
                metadata,
                attachments,
            ));

        let interrupted_processing = self.is_processing;
        let interrupted_debounce = self.is_debouncing;

        if interrupted_processing {
            self.interrupt_requested = true;
            if let Some(token) = &self.current_interrupt {
                token.set();
            }
        }

        if interrupted_debounce {
            self.debounce_notify.notify_waiters();
        }

        (interrupted_processing, interrupted_debounce)
    }

    pub fn get_combined_message(
        &mut self,
    ) -> (String, serde_json::Map<String, Value>, Vec<LlmAttachment>) {
        if self.pending_messages.is_empty() {
            return (String::new(), serde_json::Map::new(), Vec::new());
        }

        if self.pending_messages.len() == 1 {
            let message = self.pending_messages.pop_front().expect("pending message");
            return (message.content, message.metadata, message.attachments);
        }

        let mut contents = Vec::with_capacity(self.pending_messages.len());
        let mut metadata = serde_json::Map::new();
        let mut attachments = Vec::new();
        while let Some(message) = self.pending_messages.pop_front() {
            contents.push(message.content);
            metadata.extend(message.metadata);
            attachments.extend(message.attachments);
        }

        (contents.join("\n\n"), metadata, attachments)
    }

    pub fn check_interrupt(&mut self) -> bool {
        let interrupted = self.interrupt_requested
            || self
                .current_interrupt
                .as_ref()
                .is_some_and(InterruptToken::is_interrupted);
        if interrupted {
            self.interrupt_requested = false;
            if let Some(token) = &self.current_interrupt {
                token.flag.store(false, Ordering::SeqCst);
            }
        }
        interrupted
    }

    pub fn pending_count(&self) -> usize {
        self.pending_messages.len()
    }

    pub fn is_processing(&self) -> bool {
        self.is_processing
    }

    pub fn is_debouncing(&self) -> bool {
        self.is_debouncing
    }
}

#[derive(Clone, Debug)]
pub struct ConversationManager {
    states: Arc<Mutex<HashMap<i64, Arc<Mutex<ConversationState>>>>>,
    debounce: Duration,
}

impl ConversationManager {
    pub fn new(debounce: Duration) -> Self {
        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
            debounce,
        }
    }

    pub fn with_default_debounce() -> Self {
        Self::new(Duration::from_secs_f64(DEFAULT_DEBOUNCE_SECONDS))
    }

    pub async fn add_message(
        &self,
        chat_id: i64,
        user_id: i64,
        content: impl Into<String>,
        metadata: Option<serde_json::Map<String, Value>>,
        process_callback: Option<ProcessCallback>,
    ) -> bool {
        self.add_message_with_attachments(
            chat_id,
            user_id,
            content,
            metadata,
            Vec::new(),
            process_callback,
        )
        .await
    }

    pub async fn add_message_with_attachments(
        &self,
        chat_id: i64,
        user_id: i64,
        content: impl Into<String>,
        metadata: Option<serde_json::Map<String, Value>>,
        attachments: Vec<LlmAttachment>,
        process_callback: Option<ProcessCallback>,
    ) -> bool {
        let state = self.get_or_create_state(chat_id, user_id).await;
        let mut state_guard = state.lock().await;
        let (interrupted_processing, interrupted_debounce) =
            state_guard.add_message_with_attachments(content, metadata, attachments);

        if interrupted_processing || interrupted_debounce {
            return true;
        }

        if let Some(callback) = process_callback
            && !state_guard.is_processing
            && !state_guard.is_debouncing
        {
            state_guard.is_processing = true;
            state_guard.current_task = Some(self.spawn_process_loop(chat_id, callback));
        }

        true
    }

    pub async fn is_processing(&self, chat_id: i64) -> bool {
        let Some(state) = self.state(chat_id).await else {
            return false;
        };
        state.lock().await.is_processing
    }

    pub async fn is_debouncing(&self, chat_id: i64) -> bool {
        let Some(state) = self.state(chat_id).await else {
            return false;
        };
        state.lock().await.is_debouncing
    }

    pub async fn pending_count(&self, chat_id: i64) -> usize {
        let Some(state) = self.state(chat_id).await else {
            return 0;
        };
        state.lock().await.pending_count()
    }

    pub async fn cancel(&self, chat_id: i64) -> bool {
        let Some(state) = self.state(chat_id).await else {
            return false;
        };
        let mut state = state.lock().await;
        let cancelled = state.current_task.is_some() || state.debounce_task.is_some();
        if let Some(handle) = state.current_task.take() {
            handle.abort();
        }
        if let Some(handle) = state.debounce_task.take() {
            handle.abort();
        }
        state.pending_messages.clear();
        state.is_processing = false;
        state.is_debouncing = false;
        state.interrupt_requested = false;
        state.current_interrupt = None;
        cancelled
    }

    fn spawn_process_loop(&self, chat_id: i64, callback: ProcessCallback) -> AbortHandle {
        let manager = self.clone();
        tokio::spawn(manager.process_loop(chat_id, callback)).abort_handle()
    }

    fn spawn_debounce(&self, chat_id: i64, callback: ProcessCallback) -> AbortHandle {
        let manager = self.clone();
        tokio::spawn(manager.debounce_and_process(chat_id, callback)).abort_handle()
    }

    fn process_loop(self, chat_id: i64, callback: ProcessCallback) -> TaskFuture {
        Box::pin(async move {
            loop {
                let Some(state) = self.state(chat_id).await else {
                    return;
                };
                let context = {
                    let mut state = state.lock().await;
                    if state.pending_messages.is_empty() {
                        state.is_processing = false;
                        state.current_task = None;
                        return;
                    }

                    let (message, metadata, attachments) = state.get_combined_message();
                    if message.is_empty() {
                        state.is_processing = false;
                        state.current_task = None;
                        return;
                    }

                    let interrupt = InterruptToken::default();
                    state.current_interrupt = Some(interrupt.clone());
                    state.interrupt_requested = false;
                    ProcessContext {
                        chat_id: state.chat_id,
                        user_id: state.user_id,
                        message,
                        metadata,
                        attachments,
                        interrupt,
                    }
                };

                if let Err(error) = callback(context.clone()).await {
                    tracing::warn!(chat_id, error = ?error, "conversation processing callback failed");
                }

                let mut state = state.lock().await;
                let interrupted = state.interrupt_requested || context.interrupt.is_interrupted();
                state.current_interrupt = None;
                state.interrupt_requested = false;

                if interrupted && !state.pending_messages.is_empty() {
                    if self.debounce.is_zero() {
                        continue;
                    }
                    state.is_processing = false;
                    state.current_task = None;
                    state.is_debouncing = true;
                    state.debounce_task = Some(self.spawn_debounce(chat_id, callback.clone()));
                    return;
                }
            }
        })
    }

    fn debounce_and_process(self, chat_id: i64, callback: ProcessCallback) -> TaskFuture {
        Box::pin(async move {
            loop {
                let Some(state) = self.state(chat_id).await else {
                    return;
                };
                let notify = {
                    let state = state.lock().await;
                    state.debounce_notify.clone()
                };

                if tokio::time::timeout(self.debounce, notify.notified())
                    .await
                    .is_ok()
                {
                    continue;
                }

                let mut state = state.lock().await;
                state.is_debouncing = false;
                state.debounce_task = None;
                if state.pending_messages.is_empty() {
                    return;
                }

                state.is_processing = true;
                state.current_task = Some(self.spawn_process_loop(chat_id, callback.clone()));
                return;
            }
        })
    }

    async fn get_or_create_state(
        &self,
        chat_id: i64,
        user_id: i64,
    ) -> Arc<Mutex<ConversationState>> {
        let mut states = self.states.lock().await;
        states
            .entry(chat_id)
            .or_insert_with(|| Arc::new(Mutex::new(ConversationState::new(chat_id, user_id))))
            .clone()
    }

    async fn state(&self, chat_id: i64) -> Option<Arc<Mutex<ConversationState>>> {
        self.states.lock().await.get(&chat_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use tokio::time::{sleep, timeout};

    use super::*;

    #[test]
    fn state_combines_pending_messages_and_metadata() {
        let mut state = ConversationState::new(1, 2);
        state.add_message(
            "first",
            Some(serde_json::Map::from_iter([("a".to_string(), json!(1))])),
        );
        state.add_message(
            "second",
            Some(serde_json::Map::from_iter([
                ("a".to_string(), json!(2)),
                ("b".to_string(), json!(3)),
            ])),
        );

        let (combined, metadata, attachments) = state.get_combined_message();
        assert_eq!(combined, "first\n\nsecond");
        assert_eq!(metadata.get("a"), Some(&json!(2)));
        assert_eq!(metadata.get("b"), Some(&json!(3)));
        assert!(attachments.is_empty());
        assert_eq!(state.pending_count(), 0);
    }

    #[test]
    fn state_combines_attachments_in_order() {
        let mut state = ConversationState::new(1, 2);
        state.add_message_with_attachments(
            "first",
            None,
            vec![LlmAttachment {
                content_type: "image/png".to_string(),
                base64_content: "a".to_string(),
                name: Some("one.png".to_string()),
            }],
        );
        state.add_message_with_attachments(
            "second",
            None,
            vec![LlmAttachment {
                content_type: "image/jpeg".to_string(),
                base64_content: "b".to_string(),
                name: Some("two.jpg".to_string()),
            }],
        );

        let (combined, _metadata, attachments) = state.get_combined_message();

        assert_eq!(combined, "first\n\nsecond");
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].name.as_deref(), Some("one.png"));
        assert_eq!(attachments[1].name.as_deref(), Some("two.jpg"));
    }

    #[test]
    fn state_signals_processing_and_debounce_interrupts() {
        let mut state = ConversationState::new(1, 2);
        state.is_processing = true;
        let token = InterruptToken::default();
        state.current_interrupt = Some(token.clone());
        assert_eq!(state.add_message("interrupt", None), (true, false));
        assert!(token.is_interrupted());
        assert!(state.check_interrupt());
        assert!(!state.check_interrupt());

        state.is_processing = false;
        state.is_debouncing = true;
        assert_eq!(state.add_message("debounce", None), (false, true));
    }

    #[tokio::test]
    async fn manager_restarts_interrupted_work_after_debounce() {
        let manager = ConversationManager::new(Duration::from_millis(10));
        let started = Arc::new(Notify::new());
        let messages = Arc::new(Mutex::new(Vec::<String>::new()));
        let call_count = Arc::new(AtomicUsize::new(0));

        let callback: ProcessCallback = {
            let started = started.clone();
            let messages = messages.clone();
            let call_count = call_count.clone();
            Arc::new(move |context: ProcessContext| {
                let started = started.clone();
                let messages = messages.clone();
                let call_count = call_count.clone();
                Box::pin(async move {
                    let index = call_count.fetch_add(1, Ordering::SeqCst);
                    messages.lock().await.push(context.message.clone());
                    if index == 0 {
                        started.notify_waiters();
                        while !context.interrupt.is_interrupted() {
                            sleep(Duration::from_millis(1)).await;
                        }
                    }
                    Ok(())
                })
            })
        };

        manager
            .add_message(1, 2, "first", None, Some(callback.clone()))
            .await;
        timeout(Duration::from_secs(1), started.notified())
            .await
            .unwrap();
        assert!(manager.is_processing(1).await);

        manager
            .add_message(1, 2, "second", None, Some(callback.clone()))
            .await;

        timeout(Duration::from_secs(1), async {
            loop {
                if messages.lock().await.len() == 2 {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        assert_eq!(*messages.lock().await, vec!["first", "second"]);
        assert_eq!(manager.pending_count(1).await, 0);
    }

    #[tokio::test]
    async fn manager_delivers_attachments_to_process_context() {
        let manager = ConversationManager::new(Duration::from_millis(10));
        let delivered = Arc::new(Mutex::new(Vec::<LlmAttachment>::new()));
        let done = Arc::new(Notify::new());
        let callback: ProcessCallback = {
            let delivered = delivered.clone();
            let done = done.clone();
            Arc::new(move |context: ProcessContext| {
                let delivered = delivered.clone();
                let done = done.clone();
                Box::pin(async move {
                    delivered.lock().await.extend(context.attachments);
                    done.notify_waiters();
                    Ok(())
                })
            })
        };

        manager
            .add_message_with_attachments(
                1,
                2,
                "photo",
                None,
                vec![LlmAttachment {
                    content_type: "image/png".to_string(),
                    base64_content: "abc".to_string(),
                    name: Some("photo.png".to_string()),
                }],
                Some(callback),
            )
            .await;
        timeout(Duration::from_secs(1), done.notified())
            .await
            .unwrap();

        let delivered = delivered.lock().await;
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].content_type, "image/png");
        assert_eq!(delivered[0].base64_content, "abc");
        assert_eq!(delivered[0].name.as_deref(), Some("photo.png"));
    }

    #[tokio::test]
    async fn manager_cancel_aborts_processing_and_clears_pending() {
        let manager = ConversationManager::new(Duration::from_millis(10));
        let started = Arc::new(Notify::new());
        let callback: ProcessCallback = {
            let started = started.clone();
            Arc::new(move |_context: ProcessContext| {
                let started = started.clone();
                Box::pin(async move {
                    started.notify_waiters();
                    sleep(Duration::from_secs(60)).await;
                    Ok(())
                })
            })
        };

        manager
            .add_message(42, 7, "hello", None, Some(callback))
            .await;
        timeout(Duration::from_secs(1), started.notified())
            .await
            .unwrap();
        assert!(manager.cancel(42).await);
        assert!(!manager.is_processing(42).await);
        assert!(!manager.is_debouncing(42).await);
        assert_eq!(manager.pending_count(42).await, 0);
    }
}
