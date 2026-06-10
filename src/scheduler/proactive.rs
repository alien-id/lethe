use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Settings;

const DAY_SECONDS: u64 = 86_400;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProactiveRateLimiter {
    pub max_per_day: u32,
    pub cooldown_seconds: u64,
    sends: VecDeque<u64>,
}

impl ProactiveRateLimiter {
    pub fn new(max_per_day: u32, cooldown_seconds: u64) -> Self {
        Self {
            max_per_day,
            cooldown_seconds,
            sends: VecDeque::new(),
        }
    }

    pub fn from_settings(settings: &Settings) -> Self {
        Self::new(
            settings.background.proactive_max_per_day,
            u64::from(settings.background.proactive_cooldown_minutes) * 60,
        )
    }

    pub fn allowed(&mut self) -> bool {
        self.allowed_at(epoch_seconds())
    }

    pub fn record(&mut self) {
        self.record_at(epoch_seconds());
    }

    pub fn allowed_at(&mut self, now: u64) -> bool {
        self.prune(now);

        if self.max_per_day > 0 && self.sends.len() >= self.max_per_day as usize {
            return false;
        }

        if let Some(last) = self.sends.back()
            && now.saturating_sub(*last) < self.cooldown_seconds
        {
            return false;
        }

        true
    }

    pub fn record_at(&mut self, now: u64) {
        self.prune(now);
        self.sends.push_back(now);
    }

    pub fn send_count(&self) -> usize {
        self.sends.len()
    }

    fn prune(&mut self, now: u64) {
        while let Some(first) = self.sends.front() {
            if now.saturating_sub(*first) > DAY_SECONDS {
                self.sends.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Holding pen for a proactive message the rate limiter rejected. Previously
/// such messages were silently dropped — the heartbeat decided something was
/// worth telling the user, the limiter said "not now", and the insight was
/// gone. The outbox keeps the most recent suppressed message and re-offers it
/// on later ticks until the limiter allows it or it goes stale.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProactiveOutbox {
    deferred: Option<DeferredProactive>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DeferredProactive {
    message: String,
    deferred_at: u64,
}

/// A deferred proactive message older than this is stale — the moment has
/// passed and re-sending it would read as a non sequitur.
const DEFERRED_TTL_SECONDS: u64 = 6 * 60 * 60;

impl ProactiveOutbox {
    pub fn defer(&mut self, message: impl Into<String>) {
        self.defer_at(message, epoch_seconds());
    }

    pub fn defer_at(&mut self, message: impl Into<String>, now: u64) {
        // Newest wins: a fresher heartbeat insight supersedes an older one.
        self.deferred = Some(DeferredProactive {
            message: message.into(),
            deferred_at: now,
        });
    }

    pub fn take_ready(&mut self, limiter: &mut ProactiveRateLimiter) -> Option<String> {
        self.take_ready_at(limiter, epoch_seconds())
    }

    /// Pop the deferred message if the limiter would allow a send right now.
    /// Stale entries are discarded. Does NOT record the send — the caller
    /// records only after actually delivering.
    pub fn take_ready_at(
        &mut self,
        limiter: &mut ProactiveRateLimiter,
        now: u64,
    ) -> Option<String> {
        let deferred = self.deferred.as_ref()?;
        if now.saturating_sub(deferred.deferred_at) > DEFERRED_TTL_SECONDS {
            tracing::info!("dropping stale deferred proactive message");
            self.deferred = None;
            return None;
        }
        if !limiter.allowed_at(now) {
            return None;
        }
        self.deferred.take().map(|deferred| deferred.message)
    }

    pub fn is_empty(&self) -> bool {
        self.deferred.is_none()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveReminder {
    pub title: String,
    pub priority: String,
    pub due: Option<String>,
}

pub fn format_active_reminders(reminders: &[ActiveReminder], limit: usize) -> String {
    reminders
        .iter()
        .take(limit)
        .map(|todo| {
            let due = todo
                .due
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!(" (due: {value})"))
                .unwrap_or_default();
            format!("- [{}] {}{}", todo.priority, todo.title, due)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_enforces_cooldown_and_daily_limit() {
        let mut limiter = ProactiveRateLimiter::new(2, 60);
        assert!(limiter.allowed_at(1_000));
        limiter.record_at(1_000);
        assert!(!limiter.allowed_at(1_030));
        assert!(limiter.allowed_at(1_061));
        limiter.record_at(1_061);
        assert!(!limiter.allowed_at(1_200));
        assert!(limiter.allowed_at(1_000 + DAY_SECONDS + 1));
    }

    #[test]
    fn zero_daily_limit_preserves_unlimited_semantics() {
        let mut limiter = ProactiveRateLimiter::new(0, 0);
        for i in 0..10 {
            assert!(limiter.allowed_at(i));
            limiter.record_at(i);
        }
    }

    #[test]
    fn outbox_redelivers_suppressed_message_when_limiter_allows() {
        let mut limiter = ProactiveRateLimiter::new(5, 60);
        let mut outbox = ProactiveOutbox::default();

        // A send happens at t=1000; the next insight at t=1010 is inside the
        // cooldown — it gets deferred instead of dropped.
        limiter.record_at(1_000);
        assert!(!limiter.allowed_at(1_010));
        outbox.defer_at("the permit deadline is tomorrow", 1_010);
        assert!(!outbox.is_empty());

        // Still cooling down: nothing to take.
        assert_eq!(outbox.take_ready_at(&mut limiter, 1_030), None);
        assert!(!outbox.is_empty());

        // Cooldown over: the deferred message is released exactly once.
        assert_eq!(
            outbox.take_ready_at(&mut limiter, 1_061).as_deref(),
            Some("the permit deadline is tomorrow")
        );
        assert!(outbox.is_empty());
        assert_eq!(outbox.take_ready_at(&mut limiter, 1_062), None);
    }

    #[test]
    fn outbox_drops_stale_messages_and_keeps_newest() {
        let mut limiter = ProactiveRateLimiter::new(5, 60);
        let mut outbox = ProactiveOutbox::default();

        // Newest insight supersedes the older deferred one.
        outbox.defer_at("old insight", 1_000);
        outbox.defer_at("new insight", 2_000);
        assert_eq!(
            outbox.take_ready_at(&mut limiter, 2_001).as_deref(),
            Some("new insight")
        );

        // A message deferred more than the TTL ago is dropped, not delivered.
        outbox.defer_at("ancient news", 1_000);
        assert_eq!(
            outbox.take_ready_at(&mut limiter, 1_000 + DEFERRED_TTL_SECONDS + 1),
            None
        );
        assert!(outbox.is_empty());
    }

    #[test]
    fn active_reminders_format_matches_prompt_ready_shape() {
        let reminders = vec![
            ActiveReminder {
                title: "Submit report".to_string(),
                priority: "high".to_string(),
                due: Some("2026-05-23".to_string()),
            },
            ActiveReminder {
                title: "Archive notes".to_string(),
                priority: "normal".to_string(),
                due: None,
            },
        ];

        assert_eq!(
            format_active_reminders(&reminders, 10),
            "- [high] Submit report (due: 2026-05-23)\n- [normal] Archive notes"
        );
    }
}
