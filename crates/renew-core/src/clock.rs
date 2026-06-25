use std::sync::Mutex;
use time::OffsetDateTime;

/// Source of wall-clock time, injectable so scheduling is deterministic in tests.
pub trait Clock: Send + Sync {
    fn now_utc(&self) -> OffsetDateTime;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// A clock whose value is fixed (or advanced) explicitly, for tests.
#[derive(Debug)]
pub struct FixedClock {
    now: Mutex<OffsetDateTime>,
}

impl FixedClock {
    pub fn new(now: OffsetDateTime) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    pub fn set(&self, now: OffsetDateTime) {
        *self.now.lock().unwrap() = now;
    }

    pub fn advance(&self, by: time::Duration) {
        let mut g = self.now.lock().unwrap();
        *g += by;
    }
}

impl Clock for FixedClock {
    fn now_utc(&self) -> OffsetDateTime {
        *self.now.lock().unwrap()
    }
}

/// Format an instant the way the original sync.sh logged it: `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_utc(dt: OffsetDateTime) -> String {
    let fmt = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    dt.to_offset(time::UtcOffset::UTC)
        .format(fmt)
        .unwrap_or_else(|_| "0000-00-00T00:00:00Z".to_string())
}
