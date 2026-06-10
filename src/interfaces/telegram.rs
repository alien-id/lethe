use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::memory::message_metadata::{
    MessageKind, MessageVisibility, annotate_map, annotate_value,
};

mod formatting;
use formatting::{
    error_payload, expand_tilde, filename_from_url, image_extension_for_mime,
    is_invalid_reaction_error, markdown_to_telegram_html, safe_file_name,
};
pub use formatting::{
    image_mime_type_from_path, is_emoji_only_reply, split_telegram_messages, telegram_parse_mode,
};

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("telegram bot token is required")]
    MissingToken,
    #[error("telegram api error: {0}")]
    Api(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type TelegramResult<T> = Result<T, TelegramError>;

/// Number of recently-sent outgoing messages we remember per process so that we
/// can recognise reactions placed on Lethe's *own* messages.
const SENT_MESSAGE_LOG_CAPACITY: usize = 256;
/// Cap the stored excerpt of each outgoing message — we only need enough to
/// remind the model what it said, not the whole payload.
const SENT_MESSAGE_EXCERPT_CHARS: usize = 400;

/// A message Lethe sent on Telegram, kept just long enough to attribute later
/// reactions back to it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SentMessage {
    pub chat_id: i64,
    pub message_id: i64,
    pub text: String,
}

/// Bounded ring of [`SentMessage`]s. Telegram's `message_reaction` updates never
/// say who authored the reacted message, so we track our own outgoing messages
/// and match reactions against them.
#[derive(Debug, Default)]
pub struct SentMessageLog {
    entries: VecDeque<SentMessage>,
}

impl SentMessageLog {
    pub fn record(&mut self, chat_id: i64, message_id: i64, text: &str) {
        // A message_id is unique within a chat; drop any stale entry so an edit
        // or resend updates rather than duplicates.
        self.entries
            .retain(|entry| !(entry.chat_id == chat_id && entry.message_id == message_id));
        self.entries.push_back(SentMessage {
            chat_id,
            message_id,
            text: truncate_excerpt(text, SENT_MESSAGE_EXCERPT_CHARS),
        });
        while self.entries.len() > SENT_MESSAGE_LOG_CAPACITY {
            self.entries.pop_front();
        }
    }

    pub fn find(&self, chat_id: i64, message_id: i64) -> Option<SentMessage> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.chat_id == chat_id && entry.message_id == message_id)
            .cloned()
    }
}

pub type SharedSentMessageLog = Arc<Mutex<SentMessageLog>>;

/// Callback invoked once, with the locked-in user id, the first time the bot
/// accepts a user while in "lock to first user" mode. Used to persist the
/// binding so a restart keeps the same owner.
pub type FirstUserLockCallback = Arc<dyn Fn(i64) + Send + Sync>;

#[derive(Clone)]
pub struct TelegramClient {
    token: String,
    // Interior-mutable so the allowlist can be set at runtime — specifically the
    // "lock to the first user who messages" flow used by the hosted runtime.
    allowed_user_ids: Arc<Mutex<Vec<i64>>>,
    lock_on_first: bool,
    on_lock: Option<FirstUserLockCallback>,
    http: reqwest::Client,
    sent_messages: SharedSentMessageLog,
}

const TELEGRAM_ALLOWED_UPDATES: &str = "[\"message\",\"message_reaction\",\"callback_query\"]";
const PENDING_REPLY_KEYBOARD_TTL: Duration = Duration::from_secs(30 * 60);

impl TelegramClient {
    pub fn new(token: impl Into<String>, allowed_user_ids: Vec<i64>) -> TelegramResult<Self> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(TelegramError::MissingToken);
        }
        Ok(Self {
            token,
            allowed_user_ids: Arc::new(Mutex::new(allowed_user_ids)),
            lock_on_first: false,
            on_lock: None,
            http: reqwest::Client::new(),
            sent_messages: Arc::new(Mutex::new(SentMessageLog::default())),
        })
    }

    /// Enable "lock to the first user who messages" mode: when no allowlist is
    /// configured, the first user to reach the bot is bound in as the sole
    /// allowed user and `on_lock` is called once to persist them. Without this,
    /// an empty allowlist means *anyone* who finds the bot can talk to it.
    pub fn with_first_user_lock(mut self, on_lock: FirstUserLockCallback) -> Self {
        self.lock_on_first = true;
        self.on_lock = Some(on_lock);
        self
    }

    pub fn user_allowed(&self, user_id: i64) -> bool {
        let mut allowed = match self.allowed_user_ids.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if allowed.is_empty() {
            if self.lock_on_first {
                allowed.push(user_id);
                drop(allowed);
                if let Some(cb) = &self.on_lock {
                    cb(user_id);
                }
                return true;
            }
            return true;
        }
        allowed.contains(&user_id)
    }

    /// A shared handle to this client's outgoing-message log, so other send
    /// paths (e.g. the tool egress) can record into the same history the
    /// reaction handler reads from.
    pub fn sent_message_log(&self) -> SharedSentMessageLog {
        Arc::clone(&self.sent_messages)
    }

    fn remember_sent_message(&self, chat_id: i64, message_id: i64, text: &str) {
        if let Ok(mut log) = self.sent_messages.lock() {
            log.record(chat_id, message_id, text);
        }
    }

    /// Look up a message Lethe recently sent in this chat — used to tell whether
    /// an incoming reaction landed on one of her own messages.
    pub fn recent_sent_message(&self, chat_id: i64, message_id: i64) -> Option<SentMessage> {
        self.sent_messages
            .lock()
            .ok()
            .and_then(|log| log.find(chat_id, message_id))
    }

    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_seconds: u64,
    ) -> TelegramResult<Vec<TelegramUpdate>> {
        let mut request = self.http.get(self.method_url("getUpdates")).query(&[
            ("timeout", timeout_seconds.to_string()),
            ("allowed_updates", TELEGRAM_ALLOWED_UPDATES.to_string()),
        ]);
        if let Some(offset) = offset {
            request = request.query(&[("offset", offset.to_string())]);
        }
        let response = request
            .send()
            .await?
            .error_for_status()?
            .json::<TelegramResponse<Vec<TelegramUpdate>>>()
            .await?;
        response.into_result()
    }

    pub async fn send_message(&self, chat_id: i64, text: &str) -> TelegramResult<i64> {
        self.send_message_with_reply_markup(chat_id, text, None)
            .await
    }

    pub async fn send_message_with_reply_markup(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: Option<&TelegramReplyMarkup>,
    ) -> TelegramResult<i64> {
        let reply_markup = reply_markup.map(TelegramReplyMarkup::with_send_defaults);
        let reply_markup = reply_markup.as_ref();
        if let Some(reply_markup) = reply_markup {
            reply_markup.validate().map_err(TelegramError::Api)?;
        }
        // Convert the model's GitHub-flavored markdown to Telegram's HTML
        // subset so **bold**, lists, `code`, and links actually render.
        // Legacy `Markdown` mode only understands single-`*` and chokes on
        // GitHub markdown, silently degrading to literal asterisks. If HTML
        // still fails to parse, fall back to the original plain text.
        let html = markdown_to_telegram_html(text);
        let message_id = match self
            .send_message_with_mode(chat_id, &html, Some("HTML"), reply_markup)
            .await
        {
            Ok(id) => id,
            Err(error) if is_parse_entity_error(&error) => {
                tracing::warn!(
                    error = %error,
                    "Telegram rejected HTML parse, retrying as plain text"
                );
                self.send_message_with_mode(chat_id, text, None, reply_markup)
                    .await?
            }
            Err(error) => return Err(error),
        };
        // Remember our own outgoing messages so a later reaction on this message
        // can be recognised as a reaction to something Lethe said.
        self.remember_sent_message(chat_id, message_id, text);
        Ok(message_id)
    }

    async fn send_message_with_mode(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        reply_markup: Option<&TelegramReplyMarkup>,
    ) -> TelegramResult<i64> {
        // Telegram returns 400 with a structured `{ok:false, description:"..."}`
        // body when markdown parsing fails. Skip `error_for_status()` so the
        // description survives into [`TelegramError::Api`] for callers (e.g.
        // [`is_parse_entity_error`]) to inspect.
        let response = self
            .http
            .post(self.method_url("sendMessage"))
            .json(&SendMessageRequest {
                chat_id,
                text,
                parse_mode,
                disable_web_page_preview: true,
                reply_markup,
            })
            .send()
            .await?
            .json::<TelegramResponse<SentTelegramMessage>>()
            .await?;
        response.into_result().map(|message| message.message_id)
    }

    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> TelegramResult<bool> {
        let response = self
            .http
            .post(self.method_url("sendChatAction"))
            .json(&SendChatActionRequest { chat_id, action })
            .send()
            .await?
            .error_for_status()?
            .json::<TelegramResponse<bool>>()
            .await?;
        response.into_result()
    }

    pub async fn set_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: &str,
    ) -> TelegramResult<bool> {
        let emoji = emoji.trim();
        if emoji.is_empty() {
            return Ok(false);
        }
        let response = self
            .http
            .post(self.method_url("setMessageReaction"))
            .json(&SetReactionRequest {
                chat_id,
                message_id,
                reaction: vec![ReactionTypeEmoji {
                    reaction_type: "emoji",
                    emoji,
                }],
            })
            .send()
            .await?
            .error_for_status()?
            .json::<TelegramResponse<bool>>()
            .await?;
        match response.into_result() {
            Ok(value) => Ok(value),
            Err(TelegramError::Api(message)) if is_invalid_reaction_error(&message) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
        show_alert: bool,
    ) -> TelegramResult<bool> {
        let response = self
            .http
            .post(self.method_url("answerCallbackQuery"))
            .json(&AnswerCallbackQueryRequest {
                callback_query_id,
                text,
                show_alert,
            })
            .send()
            .await?
            .error_for_status()?
            .json::<TelegramResponse<bool>>()
            .await?;
        response.into_result()
    }

    pub async fn remove_reply_keyboard(&self, chat_id: i64) -> TelegramResult<i64> {
        let markup = TelegramReplyMarkup::ReplyKeyboardRemove(TelegramReplyKeyboardRemove {
            remove_keyboard: true,
            selective: None,
        });
        self.send_message_with_reply_markup(chat_id, "✓", Some(&markup))
            .await
    }

    pub async fn remove_inline_keyboard(
        &self,
        chat_id: i64,
        message_id: i64,
    ) -> TelegramResult<bool> {
        let response = self
            .http
            .post(self.method_url("editMessageReplyMarkup"))
            .json(&EditMessageReplyMarkupRequest {
                chat_id,
                message_id,
                reply_markup: None,
            })
            .send()
            .await?
            .error_for_status()?
            .json::<TelegramResponse<serde_json::Value>>()
            .await?;
        response.into_result().map(|_| true)
    }

    pub async fn get_file(&self, file_id: &str) -> TelegramResult<TelegramFileInfo> {
        let response = self
            .http
            .get(self.method_url("getFile"))
            .query(&[("file_id", file_id)])
            .send()
            .await?
            .error_for_status()?
            .json::<TelegramResponse<TelegramFileInfo>>()
            .await?;
        response.into_result()
    }

    pub async fn download_file(&self, file_path: &str) -> TelegramResult<Vec<u8>> {
        let bytes = self
            .http
            .get(format!(
                "https://api.telegram.org/file/bot{}/{}",
                self.token, file_path
            ))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    fn method_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
    #[serde(default)]
    pub message_reaction: Option<TelegramReactionUpdate>,
    #[serde(default)]
    pub callback_query: Option<TelegramCallbackQuery>,
}

impl TelegramUpdate {
    pub fn incoming_text(&self) -> Option<IncomingTelegramText> {
        let message = self.message.as_ref()?;
        let from = message.from.as_ref()?;
        let text = message.text.as_ref()?.trim();
        if text.is_empty() {
            return None;
        }
        Some(IncomingTelegramText {
            update_id: self.update_id,
            chat_id: message.chat.id,
            user_id: from.id,
            message_id: message.message_id,
            text: text.to_string(),
        })
    }

    pub fn incoming_callback(&self) -> Option<IncomingTelegramCallback> {
        let callback = self.callback_query.as_ref()?;
        let user = callback.from.as_ref()?;
        let data = callback.data.as_deref()?.trim();
        if data.is_empty() {
            return None;
        }
        // `message` is absent for presses on messages older than ~48h or
        // otherwise inaccessible. We can still answer the callback and route the
        // press: in a private chat the chat id equals the user id, so fall back
        // to that and leave `message_id` unset (no inline keyboard to remove).
        let message = callback.message.as_ref();
        let chat_id = message.map_or(user.id, |message| message.chat.id);
        let message_id = message.map(|message| message.message_id);
        let button_text = message
            .and_then(|message| message.reply_markup.as_ref())
            .and_then(|markup| markup.inline_button_text_for_callback_data(data));
        Some(IncomingTelegramCallback {
            update_id: self.update_id,
            callback_query_id: callback.id.clone(),
            chat_id,
            user_id: user.id,
            message_id,
            data: data.to_string(),
            button_text,
        })
    }

    pub fn incoming_reaction(&self) -> Option<IncomingTelegramReaction> {
        let reaction = self.message_reaction.as_ref()?;
        let user = reaction.user.as_ref()?;
        let emojis = reaction
            .new_reaction
            .iter()
            .filter_map(|reaction| reaction.emoji.as_deref())
            .filter(|emoji| !emoji.trim().is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if emojis.is_empty() {
            return None;
        }
        Some(IncomingTelegramReaction {
            update_id: self.update_id,
            chat_id: reaction.chat.id,
            user_id: user.id,
            message_id: reaction.message_id,
            emojis,
        })
    }

    pub fn incoming_audio(&self) -> Option<IncomingTelegramAudio> {
        let message = self.message.as_ref()?;
        let from = message.from.as_ref()?;
        let (media, is_voice) = if let Some(voice) = message.voice.as_ref() {
            (voice, true)
        } else {
            (message.audio.as_ref()?, false)
        };
        let file_name = media.file_name.clone().unwrap_or_else(|| {
            format!(
                "telegram_{}_{}.ogg",
                if is_voice { "voice" } else { "audio" },
                media.file_id
            )
        });
        let mime_type = media.mime_type.clone().or_else(|| {
            if is_voice {
                Some("audio/ogg".to_string())
            } else {
                None
            }
        });
        Some(IncomingTelegramAudio {
            update_id: self.update_id,
            chat_id: message.chat.id,
            user_id: from.id,
            message_id: message.message_id,
            file_id: media.file_id.clone(),
            file_name,
            mime_type,
            duration: media.duration,
            file_size: media.file_size,
            is_voice,
            caption: message.caption.clone().unwrap_or_default(),
        })
    }

    pub fn incoming_photo(&self) -> Option<IncomingTelegramPhoto> {
        let message = self.message.as_ref()?;
        let from = message.from.as_ref()?;
        let photo = message
            .photo
            .as_ref()?
            .iter()
            .max_by_key(|photo| photo.width.saturating_mul(photo.height))?;
        Some(IncomingTelegramPhoto {
            update_id: self.update_id,
            chat_id: message.chat.id,
            user_id: from.id,
            message_id: message.message_id,
            file_id: photo.file_id.clone(),
            file_unique_id: photo.file_unique_id.clone(),
            width: photo.width,
            height: photo.height,
            file_size: photo.file_size,
            caption: message.caption.clone().unwrap_or_default(),
        })
    }

    pub fn incoming_document(&self) -> Option<IncomingTelegramDocument> {
        let message = self.message.as_ref()?;
        let from = message.from.as_ref()?;
        let document = message.document.as_ref()?;
        Some(IncomingTelegramDocument {
            update_id: self.update_id,
            chat_id: message.chat.id,
            user_id: from.id,
            message_id: message.message_id,
            file_id: document.file_id.clone(),
            file_name: safe_file_name(
                document
                    .file_name
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("telegram_document"),
            ),
            mime_type: document.mime_type.clone(),
            file_size: document.file_size,
            caption: message.caption.clone().unwrap_or_default(),
        })
    }

    pub fn incoming_sticker(&self) -> Option<IncomingTelegramSticker> {
        let message = self.message.as_ref()?;
        let from = message.from.as_ref()?;
        let sticker = message.sticker.as_ref()?;
        Some(IncomingTelegramSticker {
            update_id: self.update_id,
            chat_id: message.chat.id,
            user_id: from.id,
            message_id: message.message_id,
            file_id: sticker.file_id.clone(),
            file_unique_id: sticker.file_unique_id.clone(),
            emoji: sticker.emoji.clone(),
            set_name: sticker.set_name.clone(),
            is_animated: sticker.is_animated,
            is_video: sticker.is_video,
            width: sticker.width,
            height: sticker.height,
            file_size: sticker.file_size,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramCallbackQuery {
    pub id: String,
    #[serde(default)]
    pub from: Option<TelegramUser>,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramReplyKeyboardRemove {
    pub remove_keyboard: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selective: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramForceReply {
    pub force_reply: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_field_placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selective: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TelegramReplyMarkup {
    InlineKeyboardMarkup(InlineKeyboardMarkup),
    ReplyKeyboardMarkup(ReplyKeyboardMarkup),
    ReplyKeyboardRemove(TelegramReplyKeyboardRemove),
    ForceReply(TelegramForceReply),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplyKeyboardMarkup {
    pub keyboard: Vec<Vec<ReplyKeyboardButton>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_persistent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resize_keyboard: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_time_keyboard: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_field_placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selective: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReplyKeyboardButton {
    Text(String),
    Button(KeyboardButton),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyboardButton {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_users: Option<KeyboardButtonRequestUsers>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_chat: Option<KeyboardButtonRequestChat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_managed_bot: Option<KeyboardButtonRequestManagedBot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_contact: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_location: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_poll: Option<KeyboardButtonPollType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_app: Option<WebAppInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_app: Option<WebAppInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_url: Option<LoginUrl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_inline_query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_inline_query_current_chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_inline_query_chosen_chat: Option<SwitchInlineQueryChosenChat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_text: Option<CopyTextButton>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_game: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pay: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WebAppInfo {
    pub url: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoginUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_write_access: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyboardButtonPollType {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub poll_type: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyboardButtonRequestUsers {
    pub request_id: i64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyboardButtonRequestChat {
    pub request_id: i64,
    pub chat_is_channel: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyboardButtonRequestManagedBot {
    pub request_id: i64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SwitchInlineQueryChosenChat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CopyTextButton {
    pub text: String,
}

impl TelegramReplyMarkup {
    pub fn reply_keyboard_button_texts(&self) -> Vec<String> {
        match self {
            Self::ReplyKeyboardMarkup(markup) => markup
                .keyboard
                .iter()
                .flat_map(|row| row.iter())
                .map(|button| match button {
                    ReplyKeyboardButton::Text(text) => text,
                    ReplyKeyboardButton::Button(button) => &button.text,
                })
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    }

    fn inline_button_text_for_callback_data(&self, data: &str) -> Option<String> {
        let Self::InlineKeyboardMarkup(markup) = self else {
            return None;
        };
        markup
            .inline_keyboard
            .iter()
            .flat_map(|row| row.iter())
            .find(|button| button.callback_data.as_deref().map(str::trim) == Some(data))
            .map(|button| button.text.clone())
    }

    fn with_send_defaults(&self) -> Self {
        let mut markup = self.clone();
        markup.apply_reply_keyboard_defaults();
        markup
    }

    fn apply_reply_keyboard_defaults(&mut self) {
        if let Self::ReplyKeyboardMarkup(markup) = self
            && markup.one_time_keyboard.is_none()
        {
            markup.one_time_keyboard = Some(true);
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::InlineKeyboardMarkup(markup) => {
                validate_keyboard_rows(&markup.inline_keyboard, |button| button.validate())
            }
            Self::ReplyKeyboardMarkup(markup) => {
                validate_placeholder(markup.input_field_placeholder.as_deref())?;
                validate_keyboard_rows(&markup.keyboard, |button| button.validate())
            }
            Self::ReplyKeyboardRemove(_) => Ok(()),
            Self::ForceReply(markup) => {
                validate_placeholder(markup.input_field_placeholder.as_deref())
            }
        }
    }
}

impl InlineKeyboardButton {
    fn validate(&self) -> Result<(), String> {
        validate_button_text(&self.text)?;
        let action_count = [
            self.url.is_some(),
            self.callback_data.is_some(),
            self.web_app.is_some(),
            self.login_url.is_some(),
            self.switch_inline_query.is_some(),
            self.switch_inline_query_current_chat.is_some(),
            self.switch_inline_query_chosen_chat.is_some(),
            self.copy_text.is_some(),
            self.callback_game.is_some(),
            self.pay.unwrap_or(false),
        ]
        .into_iter()
        .filter(|value| *value)
        .count();
        if action_count != 1 {
            return Err(
                "Inline keyboard buttons must contain exactly one action field.".to_string(),
            );
        }
        if let Some(data) = &self.callback_data {
            let len = data.len();
            if !(1..=64).contains(&len) {
                return Err("callback_data must be 1-64 UTF-8 bytes.".to_string());
            }
        }
        Ok(())
    }
}

impl ReplyKeyboardButton {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Text(text) => validate_button_text(text),
            Self::Button(button) => button.validate(),
        }
    }
}

impl KeyboardButton {
    fn validate(&self) -> Result<(), String> {
        validate_button_text(&self.text)?;
        let action_count = [
            self.request_users.is_some(),
            self.request_chat.is_some(),
            self.request_managed_bot.is_some(),
            self.request_contact.unwrap_or(false),
            self.request_location.unwrap_or(false),
            self.request_poll.is_some(),
            self.web_app.is_some(),
        ]
        .into_iter()
        .filter(|value| *value)
        .count();
        if action_count > 1 {
            return Err(
                "Reply keyboard buttons may contain at most one request/action field.".to_string(),
            );
        }
        Ok(())
    }
}

fn validate_keyboard_rows<T>(
    rows: &[Vec<T>],
    validate_button: impl Fn(&T) -> Result<(), String>,
) -> Result<(), String> {
    if rows.is_empty() {
        return Err("Keyboard must contain at least one row.".to_string());
    }
    for row in rows {
        if row.is_empty() {
            return Err("Keyboard rows must contain at least one button.".to_string());
        }
        for button in row {
            validate_button(button)?;
        }
    }
    Ok(())
}

fn validate_button_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("Keyboard button text is required.".to_string());
    }
    Ok(())
}

fn validate_placeholder(value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        let len = value.chars().count();
        if !(1..=64).contains(&len) {
            return Err("input_field_placeholder must be 1-64 characters.".to_string());
        }
    }
    Ok(())
}

pub fn parse_reply_markup_json(raw: &str) -> Result<Option<TelegramReplyMarkup>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let mut markup: TelegramReplyMarkup =
        serde_json::from_str(raw).map_err(|error| format!("Invalid reply_markup_json: {error}"))?;
    markup.apply_reply_keyboard_defaults();
    markup.validate()?;
    Ok(Some(markup))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PendingReplyKeyboardKey {
    chat_id: i64,
    user_id: i64,
    message_id: i64,
}

#[derive(Debug)]
struct PendingReplyKeyboard {
    button_texts: HashSet<String>,
    registered_at: Instant,
    expires_at: Instant,
}

fn pending_reply_keyboards()
-> &'static Mutex<HashMap<PendingReplyKeyboardKey, PendingReplyKeyboard>> {
    static PENDING: OnceLock<Mutex<HashMap<PendingReplyKeyboardKey, PendingReplyKeyboard>>> =
        OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_pending_reply_keyboard(
    chat_id: i64,
    user_id: i64,
    message_id: i64,
    reply_markup: &TelegramReplyMarkup,
) {
    let TelegramReplyMarkup::ReplyKeyboardMarkup(markup) = reply_markup else {
        return;
    };
    if markup.is_persistent == Some(true) || markup.one_time_keyboard == Some(false) {
        return;
    }
    let button_texts = reply_markup
        .reply_keyboard_button_texts()
        .into_iter()
        .collect::<HashSet<_>>();
    if button_texts.is_empty() {
        return;
    }
    if let Ok(mut pending) = pending_reply_keyboards().lock() {
        purge_expired_reply_keyboards(&mut pending);
        let now = Instant::now();
        pending.insert(
            PendingReplyKeyboardKey {
                chat_id,
                user_id,
                message_id,
            },
            PendingReplyKeyboard {
                button_texts,
                registered_at: now,
                expires_at: now + PENDING_REPLY_KEYBOARD_TTL,
            },
        );
    }
}

pub fn pending_reply_keyboard_matches(chat_id: i64, user_id: i64, text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let Ok(mut pending) = pending_reply_keyboards().lock() else {
        return false;
    };
    purge_expired_reply_keyboards(&mut pending);
    pending_reply_keyboard_match_key(&pending, chat_id, user_id, text).is_some()
}

pub fn forget_pending_reply_keyboard_match(chat_id: i64, user_id: i64, text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let Ok(mut pending) = pending_reply_keyboards().lock() else {
        return false;
    };
    purge_expired_reply_keyboards(&mut pending);
    let Some(key) = pending_reply_keyboard_match_key(&pending, chat_id, user_id, text) else {
        return false;
    };
    pending.remove(&key);
    true
}

fn pending_reply_keyboard_match_key(
    pending: &HashMap<PendingReplyKeyboardKey, PendingReplyKeyboard>,
    chat_id: i64,
    user_id: i64,
    text: &str,
) -> Option<PendingReplyKeyboardKey> {
    pending
        .iter()
        .filter(|(key, keyboard)| {
            key.chat_id == chat_id && key.user_id == user_id && keyboard.button_texts.contains(text)
        })
        .max_by_key(|(key, keyboard)| (keyboard.registered_at, key.message_id))
        .map(|(key, _)| key.clone())
}

fn purge_expired_reply_keyboards(
    pending: &mut HashMap<PendingReplyKeyboardKey, PendingReplyKeyboard>,
) {
    let now = Instant::now();
    pending.retain(|_, keyboard| keyboard.expires_at > now);
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramReactionUpdate {
    pub chat: TelegramChat,
    pub message_id: i64,
    #[serde(default)]
    pub user: Option<TelegramUser>,
    #[serde(default)]
    pub new_reaction: Vec<TelegramReaction>,
    #[serde(default)]
    pub old_reaction: Vec<TelegramReaction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramReaction {
    #[serde(default, rename = "type")]
    pub reaction_type: String,
    #[serde(default)]
    pub emoji: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: TelegramChat,
    #[serde(default)]
    pub from: Option<TelegramUser>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub photo: Option<Vec<TelegramPhotoSize>>,
    #[serde(default)]
    pub voice: Option<TelegramAudioMedia>,
    #[serde(default)]
    pub audio: Option<TelegramAudioMedia>,
    #[serde(default)]
    pub document: Option<TelegramDocumentMedia>,
    #[serde(default)]
    pub sticker: Option<TelegramStickerMedia>,
    #[serde(default)]
    pub reply_markup: Option<TelegramReplyMarkup>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramAudioMedia {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub duration: Option<i64>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<i64>,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramPhotoSize {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    pub width: i64,
    pub height: i64,
    #[serde(default)]
    pub file_size: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramDocumentMedia {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramStickerMedia {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub set_name: Option<String>,
    #[serde(default)]
    pub is_animated: bool,
    #[serde(default)]
    pub is_video: bool,
    #[serde(default)]
    pub width: Option<i64>,
    #[serde(default)]
    pub height: Option<i64>,
    #[serde(default)]
    pub file_size: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingTelegramText {
    pub update_id: i64,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingTelegramCallback {
    pub update_id: i64,
    pub callback_query_id: String,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: Option<i64>,
    pub data: String,
    pub button_text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingTelegramReaction {
    pub update_id: i64,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub emojis: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingTelegramAudio {
    pub update_id: i64,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub file_id: String,
    pub file_name: String,
    pub mime_type: Option<String>,
    pub duration: Option<i64>,
    pub file_size: Option<i64>,
    pub is_voice: bool,
    pub caption: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingTelegramPhoto {
    pub update_id: i64,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub width: i64,
    pub height: i64,
    pub file_size: Option<i64>,
    pub caption: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingTelegramDocument {
    pub update_id: i64,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub file_id: String,
    pub file_name: String,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
    pub caption: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingTelegramSticker {
    pub update_id: i64,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub emoji: Option<String>,
    pub set_name: Option<String>,
    pub is_animated: bool,
    pub is_video: bool,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub file_size: Option<i64>,
}

impl IncomingTelegramCallback {
    pub fn content(&self) -> String {
        let data = self.data.trim();
        match self
            .button_text
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
        {
            Some(label) if label == data => format!("[Telegram button pressed: {label}]"),
            Some(label) => format!("[Telegram button pressed: {label}]\nCallback data: {data}"),
            None => format!("[Telegram callback data: {data}]"),
        }
    }

    pub fn metadata_with_status(
        &self,
        callback_answered: bool,
        callback_consumed: bool,
        callback_reply_markup_removed: bool,
    ) -> serde_json::Value {
        let mut metadata = serde_json::Map::from_iter([
            ("source".to_string(), json!("telegram_callback")),
            (
                "callback_query_id".to_string(),
                json!(self.callback_query_id),
            ),
            ("chat_id".to_string(), json!(self.chat_id)),
            ("original_chat_id".to_string(), json!(self.chat_id)),
            ("user_id".to_string(), json!(self.user_id)),
            ("message_id".to_string(), json!(self.message_id)),
            ("update_id".to_string(), json!(self.update_id)),
            ("callback_data".to_string(), json!(self.data)),
            ("button_text".to_string(), json!(self.button_text)),
            ("callback_answered".to_string(), json!(callback_answered)),
            ("callback_consumed".to_string(), json!(callback_consumed)),
            (
                "callback_reply_markup_removed".to_string(),
                json!(callback_reply_markup_removed),
            ),
        ]);
        annotate_map(
            &mut metadata,
            MessageVisibility::UserVisible,
            MessageKind::Chat,
            "telegram",
        );
        serde_json::Value::Object(metadata)
    }

    pub fn metadata(&self) -> serde_json::Value {
        self.metadata_with_status(false, false, false)
    }
}

impl IncomingTelegramAudio {
    pub fn content_with_transcript(&self, transcript: &str) -> String {
        let label = if self.is_voice {
            "voice message"
        } else {
            "audio message"
        };
        let mut content = format!("[Transcribed {label}: {}]", transcript.trim());
        if !self.caption.trim().is_empty() {
            content.push_str("\nCaption: ");
            content.push_str(self.caption.trim());
        }
        content
    }

    pub fn metadata(&self, provider: &str, model: &str) -> serde_json::Value {
        annotate_value(
            json!({
                "source": "telegram_audio",
                "chat_id": self.chat_id,
                "user_id": self.user_id,
                "message_id": self.message_id,
                "file_id": self.file_id,
                "file_name": self.file_name,
                "mime_type": self.mime_type,
                "duration": self.duration,
                "file_size": self.file_size,
                "is_voice": self.is_voice,
                "is_audio": !self.is_voice,
                "transcription_provider": provider,
                "transcription_model": model,
            }),
            MessageVisibility::UserVisible,
            MessageKind::TelegramMedia,
            "telegram",
        )
    }
}

impl IncomingTelegramPhoto {
    pub fn content_text(&self) -> String {
        if self.caption.trim().is_empty() {
            format!("[Telegram photo: {}x{}]", self.width, self.height)
        } else {
            self.caption.trim().to_string()
        }
    }

    pub fn attachment_name(&self, content_type: &str) -> String {
        let extension = image_extension_for_mime(content_type).unwrap_or("jpg");
        format!("telegram_photo_{}.{}", self.file_id, extension)
    }

    pub fn metadata(&self, content_type: &str) -> serde_json::Value {
        annotate_value(
            json!({
                "source": "telegram_photo",
                "chat_id": self.chat_id,
                "user_id": self.user_id,
                "message_id": self.message_id,
                "file_id": self.file_id,
                "file_unique_id": self.file_unique_id,
                "width": self.width,
                "height": self.height,
                "file_size": self.file_size,
                "mime_type": content_type,
                "has_caption": !self.caption.trim().is_empty(),
            }),
            MessageVisibility::UserVisible,
            MessageKind::TelegramMedia,
            "telegram",
        )
    }
}

impl IncomingTelegramDocument {
    pub fn content_with_path(&self, file_path: &Path) -> String {
        let mut content = format!("[Received file: {}]", file_path.display());
        if !self.caption.trim().is_empty() {
            content.push('\n');
            content.push_str(self.caption.trim());
        }
        content
    }

    pub fn metadata(&self, file_path: &Path) -> serde_json::Value {
        annotate_value(
            json!({
                "source": "telegram_document",
                "chat_id": self.chat_id,
                "user_id": self.user_id,
                "message_id": self.message_id,
                "file_id": self.file_id,
                "file_name": self.file_name,
                "file_path": file_path.display().to_string(),
                "mime_type": self.mime_type,
                "file_size": self.file_size,
            }),
            MessageVisibility::UserVisible,
            MessageKind::TelegramMedia,
            "telegram",
        )
    }
}

impl IncomingTelegramSticker {
    pub fn content(&self) -> String {
        let mut parts = Vec::new();
        if let Some(emoji) = self
            .emoji
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            parts.push(format!("emoji=\"{emoji}\""));
        }
        if let Some(set_name) = self
            .set_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            parts.push(format!("set=\"{set_name}\""));
        }
        parts.push(format!("type={}", self.kind()));
        if let (Some(width), Some(height)) = (self.width, self.height) {
            parts.push(format!("size={width}x{height}"));
        }
        parts.push("visual description unavailable".to_string());
        format!("[Sticker received: {}]", parts.join(", "))
    }

    pub fn metadata(&self) -> serde_json::Value {
        annotate_value(
            json!({
                "source": "telegram_sticker",
                "chat_id": self.chat_id,
                "user_id": self.user_id,
                "message_id": self.message_id,
                "file_id": self.file_id,
                "file_unique_id": self.file_unique_id,
                "emoji": self.emoji,
                "set_name": self.set_name,
                "is_animated": self.is_animated,
                "is_video": self.is_video,
                "width": self.width,
                "height": self.height,
                "file_size": self.file_size,
            }),
            MessageVisibility::UserVisible,
            MessageKind::TelegramMedia,
            "telegram",
        )
    }

    fn kind(&self) -> &'static str {
        if self.is_video {
            "video"
        } else if self.is_animated {
            "animated"
        } else {
            "static"
        }
    }
}

impl IncomingTelegramReaction {
    pub fn content(&self) -> String {
        format!(
            "[Telegram reaction added: {} on message {}]",
            self.emojis.join(" "),
            self.message_id
        )
    }

    pub fn metadata(&self) -> serde_json::Value {
        annotate_value(
            json!({
                "source": "telegram_reaction",
                "chat_id": self.chat_id,
                "user_id": self.user_id,
                "message_id": self.message_id,
                "reaction_new": self.emojis,
            }),
            MessageVisibility::UserVisible,
            MessageKind::TelegramReaction,
            "telegram",
        )
    }

    /// Turn prompt used when the reaction landed on one of Lethe's own messages.
    /// It hands her the reacted text and makes silence the default: she replies
    /// only when a reply genuinely adds something.
    pub fn self_message_prompt(&self, reacted_text: &str) -> String {
        let excerpt = truncate_excerpt(reacted_text, SENT_MESSAGE_EXCERPT_CHARS);
        format!(
            "[Telegram] The user reacted {emoji} to your earlier message:\n\"{excerpt}\"\n\n\
             This is a reaction to something you said, not a new request. Most reactions are just \
             lightweight acknowledgement and need no answer. Reply only if a reply is genuinely \
             warranted — for example the reaction implies a question, disagreement, surprise, or \
             clearly invites you to continue. If a reply would add nothing, return an empty \
             message and stay silent. A single emoji is fine when a small acknowledgement fits.",
            emoji = self.emojis.join(" "),
            excerpt = excerpt,
        )
    }

    /// Metadata for the memory record of a reaction on Lethe's own message,
    /// enriched with what she had said so the moment stays legible in recall.
    pub fn self_message_metadata(&self, reacted_text: &str) -> serde_json::Value {
        annotate_value(
            json!({
                "source": "telegram_reaction",
                "chat_id": self.chat_id,
                "user_id": self.user_id,
                "message_id": self.message_id,
                "reaction_new": self.emojis,
                "self_message": true,
                "reacted_message_excerpt": truncate_excerpt(reacted_text, SENT_MESSAGE_EXCERPT_CHARS),
            }),
            MessageVisibility::UserVisible,
            MessageKind::TelegramReaction,
            "telegram",
        )
    }
}

/// Collapse whitespace and clamp `text` to at most `max_chars` characters,
/// appending an ellipsis when truncated. Used for outgoing-message excerpts.
fn truncate_excerpt(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut truncated: String = collapsed.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

impl<T> TelegramResponse<T> {
    fn into_result(self) -> TelegramResult<T> {
        if self.ok {
            self.result
                .ok_or_else(|| TelegramError::Api("missing result".to_string()))
        } else {
            Err(TelegramError::Api(
                self.description
                    .unwrap_or_else(|| "unknown telegram error".to_string()),
            ))
        }
    }
}

#[derive(Debug, Serialize)]
struct SendMessageRequest<'a> {
    chat_id: i64,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'a str>,
    disable_web_page_preview: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<&'a TelegramReplyMarkup>,
}

/// Telegram returns 400 with a "can't parse entities" message when the
/// markdown is malformed. We retry without parse_mode in that case so the
/// user still gets the message body, even if formatting is dropped.
fn is_parse_entity_error(error: &TelegramError) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("can't parse entities")
        || message.contains("can't find end")
        || message.contains("unsupported start tag")
        || (message.contains("400") && message.contains("entities"))
}

#[derive(Debug, Serialize)]
struct AnswerCallbackQueryRequest<'a> {
    callback_query_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    show_alert: bool,
}

#[derive(Debug, Serialize)]
struct EditMessageReplyMarkupRequest {
    chat_id: i64,
    message_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<InlineKeyboardMarkup>,
}

#[derive(Debug, Serialize)]
struct SendChatActionRequest<'a> {
    chat_id: i64,
    action: &'a str,
}

#[derive(Debug, Default, Deserialize)]
struct SentTelegramMessage {
    message_id: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct TelegramFileInfo {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub file_size: Option<i64>,
    pub file_path: String,
}

#[derive(Debug, Serialize)]
struct SetReactionRequest<'a> {
    chat_id: i64,
    message_id: i64,
    reaction: Vec<ReactionTypeEmoji<'a>>,
}

#[derive(Debug, Serialize)]
struct ReactionTypeEmoji<'a> {
    #[serde(rename = "type")]
    reaction_type: &'a str,
    emoji: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisibleTelegramChannel {
    Reaction,
    Reply,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReaction {
    pub chat_id: i64,
    pub message_id: i64,
    pub emoji: String,
}

#[derive(Debug)]
pub struct TelegramTurnGuard {
    pending_reactions: Vec<PendingReaction>,
    forced_channel: Option<VisibleTelegramChannel>,
}

impl TelegramTurnGuard {
    pub fn new() -> Self {
        Self {
            pending_reactions: Vec::new(),
            forced_channel: None,
        }
    }

    pub fn with_forced_channel(channel: VisibleTelegramChannel) -> Self {
        Self {
            pending_reactions: Vec::new(),
            forced_channel: Some(channel),
        }
    }

    pub fn queue_pending_reaction(&mut self, chat_id: i64, message_id: i64, emoji: &str) {
        self.pending_reactions.push(PendingReaction {
            chat_id,
            message_id,
            emoji: emoji.to_string(),
        });
    }

    pub fn has_pending_reactions(&self) -> bool {
        !self.pending_reactions.is_empty()
    }

    pub fn drain_pending_reactions(&mut self) -> Vec<PendingReaction> {
        std::mem::take(&mut self.pending_reactions)
    }

    pub fn choose_visible_channel(&self) -> VisibleTelegramChannel {
        self.forced_channel.unwrap_or_else(|| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.subsec_nanos())
                .unwrap_or(0);
            if nanos.is_multiple_of(2) {
                VisibleTelegramChannel::Reaction
            } else {
                VisibleTelegramChannel::Reply
            }
        })
    }
}

impl Default for TelegramTurnGuard {
    fn default() -> Self {
        Self::new()
    }
}

pub type SharedTelegramTurnGuard = Arc<Mutex<TelegramTurnGuard>>;

const TELEGRAM_TOOL_TYPING_REFRESH_SECONDS: u64 = 3;

/// [`crate::tools::registry::TurnObserver`] that keeps the Telegram "typing"
/// indicator alive for the duration of every tool call.
#[derive(Clone, Debug)]
pub struct TelegramTypingObserver {
    token: String,
    chat_id: i64,
}

impl TelegramTypingObserver {
    pub fn new(token: impl Into<String>, chat_id: i64) -> Self {
        Self {
            token: token.into(),
            chat_id,
        }
    }
}

impl crate::tools::registry::TurnObserver for TelegramTypingObserver {
    fn wrap_tool_call<'a>(
        &'a self,
        _name: &'a str,
        inner: crate::tools::registry::BoxToolFuture<'a>,
    ) -> crate::tools::registry::BoxToolFuture<'a> {
        let token = self.token.clone();
        let chat_id = self.chat_id;
        Box::pin(async move {
            let Ok(client) = TelegramClient::new(token, Vec::new()) else {
                return inner.await;
            };
            let _ = client.send_chat_action(chat_id, "typing").await;
            let typing_task = tokio::spawn(typing_refresh_loop(client, chat_id));
            let output = inner.await;
            typing_task.abort();
            let _ = typing_task.await;
            output
        })
    }
}

async fn typing_refresh_loop(client: TelegramClient, chat_id: i64) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(
            TELEGRAM_TOOL_TYPING_REFRESH_SECONDS,
        ))
        .await;
        if let Err(error) = client.send_chat_action(chat_id, "typing").await {
            tracing::debug!(chat_id, error = %error, "telegram tool typing action failed");
            return;
        }
    }
}

#[derive(Clone, Debug)]
pub struct TelegramToolContext {
    pub token: String,
    pub chat_id: i64,
    pub user_id: Option<i64>,
    pub last_message_id: Option<i64>,
    pub guard: Option<SharedTelegramTurnGuard>,
    pub dry_run: bool,
    /// Shared outgoing-message log (from the polling [`TelegramClient`]) so that
    /// messages Lethe sends via tools are also attributable to later reactions.
    pub sent_messages: Option<SharedSentMessageLog>,
}

impl TelegramToolContext {
    fn remember_sent_message(&self, message_id: i64, text: &str) {
        if message_id <= 0 {
            return;
        }
        if let Some(log) = &self.sent_messages
            && let Ok(mut log) = log.lock()
        {
            log.record(self.chat_id, message_id, text);
        }
    }
}

impl crate::tools::registry::MessageEgress for TelegramToolContext {
    fn send_message(
        &self,
        text: &str,
        parse_mode: &str,
        reply_markup_json: Option<&str>,
    ) -> String {
        Self::send_message(self, text, parse_mode, reply_markup_json)
    }
    fn send_file(&self, file_path_or_url: &str, caption: &str, as_document: bool) -> String {
        Self::send_file(self, file_path_or_url, caption, as_document)
    }
    fn react(&self, emoji: &str, message_id: i64) -> String {
        Self::react(self, emoji, message_id)
    }
}

impl TelegramToolContext {
    pub fn send_message(
        &self,
        text: &str,
        parse_mode: &str,
        reply_markup_json: Option<&str>,
    ) -> String {
        let text = text.trim();
        if text.is_empty() {
            return error_payload("Telegram message text is required.");
        }
        let reply_markup = match reply_markup_json.map(parse_reply_markup_json).transpose() {
            Ok(value) => value.flatten(),
            Err(error) => return error_payload(&error),
        };
        let parse_mode = telegram_parse_mode(parse_mode);
        if self.dry_run {
            let mut payload = json!({
                "success": true,
                "message_id": 0,
                "chat_id": self.chat_id,
                "parse_mode": parse_mode,
            });
            if let Some(reply_markup) = &reply_markup {
                payload["reply_markup"] = json!(reply_markup);
            }
            return serde_json::to_string_pretty(&payload).unwrap();
        }
        match send_message_blocking(
            &self.token,
            self.chat_id,
            text,
            parse_mode,
            reply_markup.as_ref(),
        ) {
            Ok(message_id) => {
                self.remember_sent_message(message_id, text);
                if let (Some(reply_markup), Some(user_id)) = (&reply_markup, self.user_id) {
                    register_pending_reply_keyboard(
                        self.chat_id,
                        user_id,
                        message_id,
                        reply_markup,
                    );
                }
                serde_json::to_string_pretty(&json!({
                    "success": true,
                    "message_id": message_id,
                    "chat_id": self.chat_id,
                }))
                .unwrap()
            }
            Err(error) => error_payload(&error.to_string()),
        }
    }

    pub fn send_file(&self, file_path_or_url: &str, caption: &str, as_document: bool) -> String {
        let plan = match TelegramFilePlan::from_source(file_path_or_url, as_document) {
            Ok(plan) => plan,
            Err(error) => return error_payload(&error),
        };
        if self.dry_run {
            return serde_json::to_string_pretty(&json!({
                "success": true,
                "type": plan.send_type.as_str(),
                "filename": plan.filename,
                "chat_id": self.chat_id,
                "message_id": 0,
                "method": plan.send_type.method(),
                "source": if plan.is_url { "url" } else { "local" },
            }))
            .unwrap();
        }
        let result = if plan.is_url {
            send_file_url_blocking(&self.token, self.chat_id, &plan, caption)
        } else {
            send_file_path_blocking(&self.token, self.chat_id, &plan, caption)
        };
        match result {
            Ok(message_id) => {
                self.remember_sent_message(message_id, caption);
                serde_json::to_string_pretty(&json!({
                    "success": true,
                    "type": plan.send_type.as_str(),
                    "filename": plan.filename,
                    "chat_id": self.chat_id,
                    "message_id": message_id,
                }))
                .unwrap()
            }
            Err(error) => error_payload(&error.to_string()),
        }
    }

    pub fn react(&self, emoji: &str, message_id: i64) -> String {
        let target_message_id = if message_id > 0 {
            Some(message_id)
        } else {
            self.last_message_id
        };
        let Some(target_message_id) = target_message_id else {
            return error_payload("Telegram context not set or no message to react to.");
        };
        let emoji = emoji.trim();
        if emoji.is_empty() {
            return error_payload("Telegram reaction emoji is required.");
        }
        if let Some(guard) = &self.guard {
            match guard.lock() {
                Ok(mut guard) => {
                    guard.queue_pending_reaction(self.chat_id, target_message_id, emoji);
                    return serde_json::to_string_pretty(&json!({
                        "success": true,
                        "queued": true,
                        "emoji": emoji,
                        "message_id": target_message_id,
                    }))
                    .unwrap();
                }
                Err(error) => {
                    return error_payload(&format!("Telegram turn guard poisoned: {error}"));
                }
            }
        }

        let success =
            set_message_reaction_blocking(&self.token, self.chat_id, target_message_id, emoji)
                .unwrap_or(false);
        serde_json::to_string_pretty(&json!({
            "success": success,
            "emoji": emoji,
            "message_id": target_message_id,
        }))
        .unwrap()
    }
}

pub fn send_message_blocking(
    token: &str,
    chat_id: i64,
    text: &str,
    parse_mode: Option<&'static str>,
    reply_markup: Option<&TelegramReplyMarkup>,
) -> TelegramResult<i64> {
    let mut payload = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    if let Some(parse_mode) = parse_mode {
        payload["parse_mode"] = json!(parse_mode);
    }
    let reply_markup = reply_markup.map(TelegramReplyMarkup::with_send_defaults);
    if let Some(reply_markup) = reply_markup.as_ref() {
        reply_markup.validate().map_err(TelegramError::Api)?;
        payload["reply_markup"] = json!(reply_markup);
    }
    let response = reqwest::blocking::Client::new()
        .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
        .json(&payload)
        .send()?
        .error_for_status()?
        .json::<TelegramResponse<SentTelegramMessage>>()?;
    response.into_result().map(|message| message.message_id)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelegramFileSendType {
    Photo,
    Animation,
    Video,
    Voice,
    Audio,
    Document,
}

impl TelegramFileSendType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Animation => "animation",
            Self::Video => "video",
            Self::Voice => "voice",
            Self::Audio => "audio",
            Self::Document => "document",
        }
    }

    fn method(self) -> &'static str {
        match self {
            Self::Photo => "sendPhoto",
            Self::Animation => "sendAnimation",
            Self::Video => "sendVideo",
            Self::Voice => "sendVoice",
            Self::Audio => "sendAudio",
            Self::Document => "sendDocument",
        }
    }

    fn field(self) -> &'static str {
        self.as_str()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelegramFilePlan {
    pub original: String,
    pub filename: String,
    pub path: Option<PathBuf>,
    pub is_url: bool,
    pub send_type: TelegramFileSendType,
}

impl TelegramFilePlan {
    pub fn from_source(source: &str, as_document: bool) -> Result<Self, String> {
        let source = source.trim();
        if source.is_empty() {
            return Err("Telegram file path or URL is required.".to_string());
        }
        let is_url = source.starts_with("http://") || source.starts_with("https://");
        let (filename, path) = if is_url {
            (filename_from_url(source), None)
        } else {
            let path = expand_tilde(source);
            if !path.exists() {
                return Err(format!("File not found: {}", path.display()));
            }
            let filename = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("file")
                .to_string();
            (filename, Some(path))
        };
        let ext = Path::new(&filename)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let send_type = if as_document {
            TelegramFileSendType::Document
        } else {
            match ext.as_str() {
                "jpg" | "jpeg" | "png" | "webp" | "bmp" => TelegramFileSendType::Photo,
                "gif" => TelegramFileSendType::Animation,
                "mp4" | "avi" | "mov" | "mkv" | "webm" => TelegramFileSendType::Video,
                "ogg" => TelegramFileSendType::Voice,
                "mp3" | "wav" | "flac" | "m4a" => TelegramFileSendType::Audio,
                _ => TelegramFileSendType::Document,
            }
        };
        Ok(Self {
            original: source.to_string(),
            filename,
            path,
            is_url,
            send_type,
        })
    }
}

fn send_file_url_blocking(
    token: &str,
    chat_id: i64,
    plan: &TelegramFilePlan,
    caption: &str,
) -> TelegramResult<i64> {
    let mut payload = json!({
        "chat_id": chat_id,
        plan.send_type.field(): plan.original,
    });
    if !caption.trim().is_empty() {
        payload["caption"] = json!(caption.trim());
    }
    let response = reqwest::blocking::Client::new()
        .post(format!(
            "https://api.telegram.org/bot{token}/{}",
            plan.send_type.method()
        ))
        .json(&payload)
        .send()?
        .error_for_status()?
        .json::<TelegramResponse<SentTelegramMessage>>()?;
    response.into_result().map(|message| message.message_id)
}

fn send_file_path_blocking(
    token: &str,
    chat_id: i64,
    plan: &TelegramFilePlan,
    caption: &str,
) -> TelegramResult<i64> {
    let Some(path) = &plan.path else {
        return Err(TelegramError::Api("missing local file path".to_string()));
    };
    let mut form = reqwest::blocking::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .part(
            plan.send_type.field().to_string(),
            reqwest::blocking::multipart::Part::file(path)?,
        );
    if !caption.trim().is_empty() {
        form = form.text("caption", caption.trim().to_string());
    }
    let response = reqwest::blocking::Client::new()
        .post(format!(
            "https://api.telegram.org/bot{token}/{}",
            plan.send_type.method()
        ))
        .multipart(form)
        .send()?
        .error_for_status()?
        .json::<TelegramResponse<SentTelegramMessage>>()?;
    response.into_result().map(|message| message.message_id)
}

pub fn set_message_reaction_blocking(
    token: &str,
    chat_id: i64,
    message_id: i64,
    emoji: &str,
) -> TelegramResult<bool> {
    let emoji = emoji.trim();
    if emoji.is_empty() {
        return Ok(false);
    }
    let response = reqwest::blocking::Client::new()
        .post(format!(
            "https://api.telegram.org/bot{token}/setMessageReaction"
        ))
        .json(&SetReactionRequest {
            chat_id,
            message_id,
            reaction: vec![ReactionTypeEmoji {
                reaction_type: "emoji",
                emoji,
            }],
        })
        .send()?
        .error_for_status()?
        .json::<TelegramResponse<bool>>()?;
    match response.into_result() {
        Ok(value) => Ok(value),
        Err(TelegramError::Api(message)) if is_invalid_reaction_error(&message) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_update_and_authorization() {
        let raw = r#"{
            "update_id": 42,
            "message": {
                "message_id": 5,
                "chat": {"id": 100},
                "from": {"id": 7},
                "text": " hello "
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_text().unwrap();
        assert_eq!(incoming.update_id, 42);
        assert_eq!(incoming.chat_id, 100);
        assert_eq!(incoming.user_id, 7);
        assert_eq!(incoming.message_id, 5);
        assert_eq!(incoming.text, "hello");

        let client = TelegramClient::new("token", vec![7]).unwrap();
        assert!(client.user_allowed(7));
        assert!(!client.user_allowed(8));
    }

    #[test]
    fn empty_allowlist_without_lock_allows_anyone() {
        let client = TelegramClient::new("token", Vec::new()).unwrap();
        assert!(client.user_allowed(1));
        assert!(client.user_allowed(2));
    }

    #[test]
    fn first_user_lock_binds_first_user_then_rejects_others() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let captured = Arc::new(AtomicI64::new(0));
        let sink = captured.clone();
        let client = TelegramClient::new("token", Vec::new())
            .unwrap()
            .with_first_user_lock(Arc::new(move |uid| sink.store(uid, Ordering::SeqCst)));
        // First user to message locks in (and is persisted via the callback).
        assert!(client.user_allowed(111));
        assert_eq!(captured.load(Ordering::SeqCst), 111);
        // The bound owner stays allowed; everyone else is rejected.
        assert!(client.user_allowed(111));
        assert!(!client.user_allowed(222));
    }

    #[test]
    fn parses_reaction_update_as_synthetic_context() {
        let raw = r#"{
            "update_id": 43,
            "message_reaction": {
                "chat": {"id": 100},
                "message_id": 5,
                "user": {"id": 7},
                "old_reaction": [],
                "new_reaction": [{"type": "emoji", "emoji": "🔥"}]
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_reaction().unwrap();

        assert_eq!(incoming.update_id, 43);
        assert_eq!(incoming.chat_id, 100);
        assert_eq!(incoming.user_id, 7);
        assert_eq!(incoming.message_id, 5);
        assert_eq!(incoming.emojis, vec!["🔥"]);
        assert!(incoming.content().contains("Telegram reaction added"));
        assert_eq!(incoming.metadata()["reaction_new"][0], "🔥");
    }

    #[test]
    fn parses_callback_update_with_label_content_and_metadata() {
        let raw = r#"{
            "update_id": 44,
            "callback_query": {
                "id": "callback-1",
                "from": {"id": 7},
                "data": "start_now",
                "message": {
                    "message_id": 6,
                    "chat": {"id": 100},
                    "reply_markup": {
                        "inline_keyboard": [[{"text": "Start now", "callback_data": "start_now"}]]
                    }
                }
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_callback().unwrap();

        assert_eq!(incoming.update_id, 44);
        assert_eq!(incoming.chat_id, 100);
        assert_eq!(incoming.user_id, 7);
        assert_eq!(incoming.message_id, Some(6));
        assert_eq!(incoming.data, "start_now");
        assert_eq!(incoming.button_text.as_deref(), Some("Start now"));
        assert_eq!(
            incoming.content(),
            "[Telegram button pressed: Start now]\nCallback data: start_now"
        );
        let metadata = incoming.metadata_with_status(true, true, true);
        assert_eq!(metadata["callback_data"], "start_now");
        assert_eq!(metadata["button_text"], "Start now");
        assert_eq!(metadata["callback_consumed"], true);
    }

    #[test]
    fn sent_message_log_records_and_matches_recent_messages() {
        let client = TelegramClient::new("token", vec![7]).unwrap();
        assert!(client.recent_sent_message(100, 5).is_none());

        client.remember_sent_message(100, 5, "hello there");
        let found = client
            .recent_sent_message(100, 5)
            .expect("recorded message");
        assert_eq!(found.chat_id, 100);
        assert_eq!(found.message_id, 5);
        assert_eq!(found.text, "hello there");

        // message_id is matched per-chat.
        assert!(client.recent_sent_message(101, 5).is_none());
    }

    #[test]
    fn sent_message_log_evicts_oldest_beyond_capacity() {
        let mut log = SentMessageLog::default();
        for id in 0..(SENT_MESSAGE_LOG_CAPACITY as i64 + 10) {
            log.record(1, id, "msg");
        }
        // The earliest entries fall out of the bounded ring.
        assert!(log.find(1, 0).is_none());
        assert!(log.find(1, 9).is_none());
        // The most recent ones are retained.
        assert!(log.find(1, SENT_MESSAGE_LOG_CAPACITY as i64 + 9).is_some());
    }

    #[test]
    fn self_message_prompt_includes_emoji_and_silence_guidance() {
        let reaction = IncomingTelegramReaction {
            update_id: 1,
            chat_id: 100,
            user_id: 7,
            message_id: 5,
            emojis: vec!["🔥".to_string()],
        };
        let prompt = reaction.self_message_prompt("a message I sent earlier");
        assert!(prompt.contains("🔥"));
        assert!(prompt.contains("a message I sent earlier"));
        assert!(prompt.contains("stay silent"));

        let metadata = reaction.self_message_metadata("a message I sent earlier");
        assert_eq!(metadata["self_message"], true);
        assert_eq!(
            metadata["reacted_message_excerpt"],
            "a message I sent earlier"
        );
    }

    #[test]
    fn truncate_excerpt_collapses_whitespace_and_clamps() {
        assert_eq!(truncate_excerpt("  a\n\n b   c ", 100), "a b c");
        let long = "x".repeat(10);
        assert_eq!(truncate_excerpt(&long, 4), "xxxx…");
    }

    #[test]
    fn parses_voice_update_for_transcription() {
        let raw = r#"{
            "update_id": 44,
            "message": {
                "message_id": 6,
                "chat": {"id": 100},
                "from": {"id": 7},
                "caption": "context",
                "voice": {
                    "file_id": "voice-file",
                    "file_unique_id": "voice-unique",
                    "duration": 3,
                    "mime_type": "audio/ogg",
                    "file_size": 1234
                }
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_audio().unwrap();

        assert!(incoming.is_voice);
        assert_eq!(incoming.chat_id, 100);
        assert_eq!(incoming.user_id, 7);
        assert_eq!(incoming.message_id, 6);
        assert_eq!(incoming.file_id, "voice-file");
        assert_eq!(incoming.file_name, "telegram_voice_voice-file.ogg");
        assert_eq!(incoming.mime_type.as_deref(), Some("audio/ogg"));
        assert_eq!(incoming.duration, Some(3));
        assert_eq!(incoming.file_size, Some(1234));
        assert_eq!(
            incoming.content_with_transcript("hello"),
            "[Transcribed voice message: hello]\nCaption: context"
        );
        let metadata = incoming.metadata("auto", "default");
        assert_eq!(metadata["is_voice"], true);
        assert_eq!(metadata["is_audio"], false);
        assert_eq!(metadata["file_name"], "telegram_voice_voice-file.ogg");
    }

    #[test]
    fn parses_audio_update_with_filename() {
        let raw = r#"{
            "update_id": 45,
            "message": {
                "message_id": 7,
                "chat": {"id": 101},
                "from": {"id": 8},
                "audio": {
                    "file_id": "audio-file",
                    "file_unique_id": "audio-unique",
                    "duration": 10,
                    "mime_type": "audio/mpeg",
                    "file_name": "song.mp3",
                    "file_size": 5678
                }
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_audio().unwrap();

        assert!(!incoming.is_voice);
        assert_eq!(incoming.chat_id, 101);
        assert_eq!(incoming.user_id, 8);
        assert_eq!(incoming.file_name, "song.mp3");
        assert_eq!(incoming.mime_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(
            incoming.content_with_transcript("music"),
            "[Transcribed audio message: music]"
        );
    }

    #[test]
    fn parses_photo_update_for_multimodal_turn() {
        let raw = r#"{
            "update_id": 46,
            "message": {
                "message_id": 8,
                "chat": {"id": 102},
                "from": {"id": 9},
                "caption": "what is on this bench?",
                "photo": [
                    {
                        "file_id": "small-photo",
                        "file_unique_id": "small-unique",
                        "width": 90,
                        "height": 90,
                        "file_size": 111
                    },
                    {
                        "file_id": "large-photo",
                        "file_unique_id": "large-unique",
                        "width": 1280,
                        "height": 720,
                        "file_size": 222
                    }
                ]
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_photo().unwrap();

        assert_eq!(incoming.update_id, 46);
        assert_eq!(incoming.chat_id, 102);
        assert_eq!(incoming.user_id, 9);
        assert_eq!(incoming.file_id, "large-photo");
        assert_eq!(incoming.width, 1280);
        assert_eq!(incoming.height, 720);
        assert_eq!(incoming.content_text(), "what is on this bench?");
        assert_eq!(
            incoming.attachment_name("image/png"),
            "telegram_photo_large-photo.png"
        );
        assert_eq!(image_mime_type_from_path("photos/image.webp"), "image/webp");
        assert_eq!(incoming.metadata("image/png")["source"], "telegram_photo");
    }

    #[test]
    fn parses_document_update_with_safe_filename() {
        let raw = r#"{
            "update_id": 47,
            "message": {
                "message_id": 8,
                "chat": {"id": 102},
                "from": {"id": 9},
                "caption": "please review",
                "document": {
                    "file_id": "doc-file",
                    "file_unique_id": "doc-unique",
                    "file_name": "../report.pdf",
                    "mime_type": "application/pdf",
                    "file_size": 999
                }
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_document().unwrap();

        assert_eq!(incoming.chat_id, 102);
        assert_eq!(incoming.user_id, 9);
        assert_eq!(incoming.file_id, "doc-file");
        assert_eq!(incoming.file_name, "report.pdf");
        assert_eq!(incoming.mime_type.as_deref(), Some("application/pdf"));
        assert_eq!(incoming.file_size, Some(999));
        assert!(
            incoming
                .content_with_path(Path::new("/tmp/report.pdf"))
                .contains("please review")
        );
    }

    #[test]
    fn parses_sticker_update_as_text_context() {
        let raw = r#"{
            "update_id": 48,
            "message": {
                "message_id": 9,
                "chat": {"id": 103},
                "from": {"id": 10},
                "sticker": {
                    "file_id": "sticker-file",
                    "file_unique_id": "sticker-unique",
                    "emoji": "🔥",
                    "set_name": "hot",
                    "is_animated": false,
                    "is_video": true,
                    "width": 512,
                    "height": 512,
                    "file_size": 321
                }
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_sticker().unwrap();

        assert_eq!(incoming.chat_id, 103);
        assert_eq!(incoming.user_id, 10);
        assert_eq!(incoming.file_id, "sticker-file");
        assert!(incoming.content().contains("emoji=\"🔥\""));
        assert!(incoming.content().contains("type=video"));
        assert_eq!(incoming.metadata()["is_video"], true);
    }

    #[test]
    fn emoji_only_reply_accepts_emoji_and_rejects_text() {
        for value in ["👍", "❤️", "🔥🔥", "👨‍👩‍👧‍👦", "👍🏻", "🇺🇦"]
        {
            assert!(is_emoji_only_reply(value), "{value}");
        }
        for value in ["", "   ", "👍!", "thanks 👍", "<3", "reply ❤️"] {
            assert!(!is_emoji_only_reply(value), "{value}");
        }
    }

    #[test]
    fn turn_guard_queues_drains_and_can_force_channel() {
        let mut guard = TelegramTurnGuard::with_forced_channel(VisibleTelegramChannel::Reaction);
        guard.queue_pending_reaction(99, 42, "🔥");
        guard.queue_pending_reaction(99, 43, "👍");

        assert!(guard.has_pending_reactions());
        assert_eq!(
            guard.choose_visible_channel(),
            VisibleTelegramChannel::Reaction
        );
        assert_eq!(
            guard.drain_pending_reactions(),
            vec![
                PendingReaction {
                    chat_id: 99,
                    message_id: 42,
                    emoji: "🔥".to_string(),
                },
                PendingReaction {
                    chat_id: 99,
                    message_id: 43,
                    emoji: "👍".to_string(),
                },
            ]
        );
        assert!(!guard.has_pending_reactions());
    }

    #[test]
    fn telegram_tool_context_queues_reaction_when_guarded() {
        let guard = Arc::new(Mutex::new(TelegramTurnGuard::new()));
        let context = TelegramToolContext {
            token: "token".to_string(),
            chat_id: 99,
            user_id: Some(7),
            last_message_id: Some(42),
            guard: Some(guard.clone()),
            dry_run: true,
            sent_messages: None,
        };

        let payload = context.react("🔥", 77);
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["success"], true);
        assert_eq!(value["queued"], true);
        assert_eq!(value["message_id"], 77);

        let pending = guard.lock().unwrap().drain_pending_reactions();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].chat_id, 99);
        assert_eq!(pending[0].message_id, 77);
        assert_eq!(pending[0].emoji, "🔥");
    }

    #[test]
    fn reply_keyboard_markup_serializes_to_bot_api_json() {
        let markup = parse_reply_markup_json(
            r#"{"keyboard":[["Yes",{"text":"No"}]],"resize_keyboard":true,"input_field_placeholder":"Pick one"}"#,
        )
        .unwrap()
        .unwrap();
        let value = serde_json::to_value(&markup).unwrap();
        assert_eq!(value["keyboard"][0][0], "Yes");
        assert_eq!(value["keyboard"][0][1]["text"], "No");
        assert_eq!(value["resize_keyboard"], true);
        assert_eq!(value["one_time_keyboard"], true);
        assert_eq!(value["input_field_placeholder"], "Pick one");
    }

    #[test]
    fn reply_keyboard_preserves_explicit_one_time_false() {
        let markup =
            parse_reply_markup_json(r#"{"keyboard":[["Yes","No"]],"one_time_keyboard":false}"#)
                .unwrap()
                .unwrap();
        let value = serde_json::to_value(&markup).unwrap();
        assert_eq!(value["one_time_keyboard"], false);
    }

    #[test]
    fn inline_keyboard_markup_serializes_to_bot_api_json() {
        let markup = parse_reply_markup_json(
            r#"{"inline_keyboard":[[{"text":"Start","callback_data":"start_now"},{"text":"Docs","url":"https://example.com"}]]}"#,
        )
        .unwrap()
        .unwrap();
        let value = serde_json::to_value(&markup).unwrap();
        assert_eq!(value["inline_keyboard"][0][0]["text"], "Start");
        assert_eq!(value["inline_keyboard"][0][0]["callback_data"], "start_now");
        assert_eq!(value["inline_keyboard"][0][1]["url"], "https://example.com");
    }

    #[test]
    fn inline_keyboard_validation_rejects_zero_or_multiple_actions() {
        let no_action =
            parse_reply_markup_json(r#"{"inline_keyboard":[[{"text":"Start"}]]}"#).unwrap_err();
        assert!(no_action.contains("exactly one action"));

        let two_actions = parse_reply_markup_json(
            r#"{"inline_keyboard":[[{"text":"Start","callback_data":"start","url":"https://example.com"}]]}"#,
        )
        .unwrap_err();
        assert!(two_actions.contains("exactly one action"));
    }

    #[test]
    fn callback_data_validation_enforces_byte_limit() {
        let empty = parse_reply_markup_json(
            r#"{"inline_keyboard":[[{"text":"Start","callback_data":""}]]}"#,
        )
        .unwrap_err();
        assert!(empty.contains("callback_data"));

        let long_data = "x".repeat(65);
        let raw = format!(
            r#"{{"inline_keyboard":[[{{"text":"Start","callback_data":"{long_data}"}}]]}}"#
        );
        let too_long = parse_reply_markup_json(&raw).unwrap_err();
        assert!(too_long.contains("callback_data"));
    }

    #[test]
    fn typed_reply_markup_send_defaults_make_keyboard_one_time() {
        let markup = TelegramReplyMarkup::ReplyKeyboardMarkup(ReplyKeyboardMarkup {
            keyboard: vec![vec![ReplyKeyboardButton::Text("Yes".to_string())]],
            is_persistent: None,
            resize_keyboard: Some(true),
            one_time_keyboard: None,
            input_field_placeholder: None,
            selective: None,
        })
        .with_send_defaults();
        let value = serde_json::to_value(&markup).unwrap();
        assert_eq!(value["one_time_keyboard"], true);
        assert_eq!(value["resize_keyboard"], true);
    }

    #[test]
    fn send_message_request_omits_and_includes_reply_markup() {
        let plain = serde_json::to_value(&SendMessageRequest {
            chat_id: 1,
            text: "hello",
            parse_mode: None,
            disable_web_page_preview: true,
            reply_markup: None,
        })
        .unwrap();
        assert!(plain.get("reply_markup").is_none());

        let markup = parse_reply_markup_json(
            r#"{"inline_keyboard":[[{"text":"Start","callback_data":"start"}]]}"#,
        )
        .unwrap()
        .unwrap();
        let with_markup = serde_json::to_value(&SendMessageRequest {
            chat_id: 1,
            text: "hello",
            parse_mode: Some("HTML"),
            disable_web_page_preview: true,
            reply_markup: Some(&markup),
        })
        .unwrap();
        assert_eq!(
            with_markup["reply_markup"]["inline_keyboard"][0][0]["text"],
            "Start"
        );
    }

    #[test]
    fn parses_callback_query_update_with_metadata() {
        let raw = r#"{
            "update_id": 49,
            "callback_query": {
                "id": "cb-1",
                "from": {"id": 7},
                "message": {
                    "message_id": 5,
                    "chat": {"id": 100},
                    "reply_markup": {"inline_keyboard":[[{"text":"Start now","callback_data":"start_now"}]]}
                },
                "data": "start_now"
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_callback().unwrap();
        assert_eq!(incoming.update_id, 49);
        assert_eq!(incoming.callback_query_id, "cb-1");
        assert_eq!(incoming.chat_id, 100);
        assert_eq!(incoming.user_id, 7);
        assert_eq!(incoming.message_id, Some(5));
        assert_eq!(incoming.data, "start_now");
        assert_eq!(incoming.button_text.as_deref(), Some("Start now"));
        let metadata = incoming.metadata_with_status(true, true, true);
        assert_eq!(metadata["source"], "telegram_callback");
        assert_eq!(metadata["callback_data"], "start_now");
        assert_eq!(metadata["button_text"], "Start now");
        assert_eq!(metadata["callback_answered"], true);
        assert_eq!(metadata["callback_consumed"], true);
        assert_eq!(metadata["callback_reply_markup_removed"], true);
    }

    #[test]
    fn parses_callback_query_without_message_falls_back_to_user_chat() {
        // Telegram omits `message` for presses on messages older than ~48h.
        // The press must still be routable (private chat id == user id) and the
        // callback still answerable, rather than silently dropped.
        let raw = r#"{
            "update_id": 50,
            "callback_query": {
                "id": "cb-2",
                "from": {"id": 7},
                "data": "start_now"
            }
        }"#;
        let update: TelegramUpdate = serde_json::from_str(raw).unwrap();
        let incoming = update.incoming_callback().unwrap();
        assert_eq!(incoming.callback_query_id, "cb-2");
        assert_eq!(incoming.chat_id, 7);
        assert_eq!(incoming.user_id, 7);
        assert_eq!(incoming.message_id, None);
        assert_eq!(incoming.data, "start_now");
        assert_eq!(incoming.button_text, None);
    }

    #[test]
    fn edit_message_reply_markup_request_omits_markup_to_remove_inline_keyboard() {
        let value = serde_json::to_value(&EditMessageReplyMarkupRequest {
            chat_id: 100,
            message_id: 5,
            reply_markup: None,
        })
        .unwrap();
        assert_eq!(value["chat_id"], 100);
        assert_eq!(value["message_id"], 5);
        assert!(value.get("reply_markup").is_none());
    }

    #[test]
    fn pending_reply_keyboard_matches_once_by_chat_user_message_and_text() {
        let chat_id = 9_000_001;
        let user_id = 7_000_001;
        let message_id = 5;
        let markup = parse_reply_markup_json(r#"{"keyboard":[["Yes","No"]]}"#)
            .unwrap()
            .unwrap();
        register_pending_reply_keyboard(chat_id, user_id, message_id, &markup);
        assert!(pending_reply_keyboard_matches(chat_id, user_id, "Yes"));
        assert!(forget_pending_reply_keyboard_match(chat_id, user_id, "Yes"));
        assert!(!pending_reply_keyboard_matches(chat_id, user_id, "Yes"));
    }

    #[test]
    fn pending_reply_keyboard_does_not_match_other_users_or_inline_markups() {
        let chat_id = 9_000_002;
        let user_id = 7_000_002;
        let reply_markup = parse_reply_markup_json(r#"{"keyboard":[["Yes","No"]]}"#)
            .unwrap()
            .unwrap();
        let inline_markup = parse_reply_markup_json(
            r#"{"inline_keyboard":[[{"text":"Yes","callback_data":"yes"}]]}"#,
        )
        .unwrap()
        .unwrap();
        let remove_markup = TelegramReplyMarkup::ReplyKeyboardRemove(TelegramReplyKeyboardRemove {
            remove_keyboard: true,
            selective: None,
        });

        register_pending_reply_keyboard(chat_id, user_id, 5, &reply_markup);
        register_pending_reply_keyboard(chat_id, user_id, 6, &inline_markup);
        register_pending_reply_keyboard(chat_id, user_id, 7, &remove_markup);

        assert!(!pending_reply_keyboard_matches(chat_id, user_id + 1, "Yes"));
        assert!(pending_reply_keyboard_matches(chat_id, user_id, "Yes"));
        assert!(forget_pending_reply_keyboard_match(chat_id, user_id, "Yes"));
        assert!(!pending_reply_keyboard_matches(chat_id, user_id, "Yes"));
    }

    #[test]
    fn persistent_or_non_one_time_reply_keyboards_are_not_pending() {
        let chat_id = 9_000_003;
        let user_id = 7_000_003;
        let persistent = parse_reply_markup_json(r#"{"keyboard":[["Stay"]],"is_persistent":true}"#)
            .unwrap()
            .unwrap();
        let not_one_time =
            parse_reply_markup_json(r#"{"keyboard":[["Stay"]],"one_time_keyboard":false}"#)
                .unwrap()
                .unwrap();

        register_pending_reply_keyboard(chat_id, user_id, 5, &persistent);
        register_pending_reply_keyboard(chat_id, user_id, 6, &not_one_time);

        assert!(!pending_reply_keyboard_matches(chat_id, user_id, "Stay"));
    }

    #[test]
    fn allowed_updates_include_callback_query() {
        assert!(TELEGRAM_ALLOWED_UPDATES.contains("callback_query"));
    }

    #[test]
    fn parse_mode_maps_supported_values() {
        assert_eq!(telegram_parse_mode("markdown"), Some("MarkdownV2"));
        assert_eq!(telegram_parse_mode("MarkdownV2"), Some("MarkdownV2"));
        assert_eq!(telegram_parse_mode("html"), Some("HTML"));
        assert_eq!(telegram_parse_mode("plain"), None);
        assert_eq!(telegram_parse_mode(""), None);
    }

    #[test]
    fn file_plan_selects_send_type_from_extension() {
        let cases = [
            (
                "https://example.com/image.png",
                false,
                TelegramFileSendType::Photo,
            ),
            (
                "https://example.com/anim.gif",
                false,
                TelegramFileSendType::Animation,
            ),
            (
                "https://example.com/movie.mp4",
                false,
                TelegramFileSendType::Video,
            ),
            (
                "https://example.com/voice.ogg",
                false,
                TelegramFileSendType::Voice,
            ),
            (
                "https://example.com/song.mp3",
                false,
                TelegramFileSendType::Audio,
            ),
            (
                "https://example.com/report.pdf",
                false,
                TelegramFileSendType::Document,
            ),
            (
                "https://example.com/image.png",
                true,
                TelegramFileSendType::Document,
            ),
        ];

        for (source, as_document, expected) in cases {
            let plan = TelegramFilePlan::from_source(source, as_document).unwrap();
            assert_eq!(plan.send_type, expected);
            assert!(plan.is_url);
        }
    }

    #[test]
    fn telegram_tool_context_dry_runs_message_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("image.png");
        std::fs::write(&file, "not real image bytes").unwrap();
        let context = TelegramToolContext {
            token: "token".to_string(),
            chat_id: 99,
            user_id: Some(7),
            last_message_id: Some(42),
            guard: None,
            dry_run: true,
            sent_messages: None,
        };

        let message: serde_json::Value = serde_json::from_str(&context.send_message(
            "hello",
            "markdown",
            Some(r#"{"inline_keyboard":[[{"text":"Start","callback_data":"start"}]]}"#),
        ))
        .unwrap();
        assert_eq!(message["success"], true);
        assert_eq!(message["chat_id"], 99);
        assert_eq!(message["parse_mode"], "MarkdownV2");
        assert_eq!(
            message["reply_markup"]["inline_keyboard"][0][0]["text"],
            "Start"
        );

        let file_payload: serde_json::Value =
            serde_json::from_str(&context.send_file(file.to_str().unwrap(), "caption", false))
                .unwrap();
        assert_eq!(file_payload["success"], true);
        assert_eq!(file_payload["type"], "photo");
        assert_eq!(file_payload["filename"], "image.png");
    }

    #[test]
    fn split_messages_splits_on_dashes_and_respects_size_limit() {
        // --- on its own line is the bubble divider (matches Python main).
        let chunks = split_telegram_messages("one\n---\ntwo");
        assert_eq!(chunks, vec!["one", "two"]);

        // Multiple bubbles separated by ---, with surrounding blank lines.
        let chunks = split_telegram_messages("one\n\n---\n\ntwo\n\n---\n\nthree");
        assert_eq!(chunks, vec!["one", "two", "three"]);

        // No --- divider → whole text is a single bubble (paragraph blanks
        // inside are preserved, not used as splitters anymore).
        let chunks = split_telegram_messages("one\n\ntwo");
        assert_eq!(chunks, vec!["one\n\ntwo"]);

        // `---` inside a fenced code block must NOT split.
        let chunks = split_telegram_messages("```\nbefore\n---\nafter\n```");
        assert_eq!(chunks, vec!["```\nbefore\n---\nafter\n```"]);

        // Markdown table separator `|---|---|` should be left alone — it's
        // not a divider line because it contains `|`.
        let chunks = split_telegram_messages("| a | b |\n|---|---|\n| 1 | 2 |");
        assert_eq!(chunks.len(), 1);

        let long = "x".repeat(5000);
        let chunks = split_telegram_messages(&long);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4096));
    }

    #[test]
    fn telegram_response_reports_api_errors() {
        let response: TelegramResponse<serde_json::Value> =
            serde_json::from_str(r#"{"ok":false,"description":"Forbidden"}"#).unwrap();
        assert!(matches!(
            response.into_result().unwrap_err(),
            TelegramError::Api(message) if message == "Forbidden"
        ));
    }
}
