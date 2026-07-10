/// Abstraction over the transports that can ship a message back to the user
/// during a tool turn. Both `TelegramToolContext` (used by the long-running
/// poller) and `ClientToolContext` (used by the SSE API) implement this so the
/// `telegram_send_*` / `telegram_react` tools dispatch through one path.
pub trait MessageEgress {
    fn send_message(&self, text: &str, parse_mode: &str, reply_markup_json: Option<&str>)
    -> String;
    fn send_file(&self, file_path_or_url: &str, caption: &str, as_document: bool) -> String;
    fn react(&self, emoji: &str, message_id: i64) -> String;
}

impl super::ToolRegistry<'_> {
    /// Pick the active message egress, preferring direct Telegram over the
    /// generic client transport when both are attached.
    pub(crate) fn message_egress(&self) -> Option<&dyn MessageEgress> {
        if let Some(context) = self.runtime.telegram.as_ref() {
            return Some(context);
        }
        if let Some(context) = self.runtime.client.as_ref() {
            return Some(context);
        }
        None
    }
}

pub(crate) const NO_EGRESS_ERROR: &str =
    "No chat transport attached. This tool only works during active user transport processing.";
