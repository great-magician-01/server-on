use crate::LogLine;
use chrono::Local;
use server_on_core::event::{LogEvent, LogStream};

pub const LOG_CAP: usize = 2_000;
pub const RECENT_CAP: usize = 30;

#[derive(Default)]
pub struct LogBuffer {
    logs: Vec<LogEvent>,
    pending: Vec<LogEvent>,
    evicted: Vec<LogEvent>,
    epoch: i32,
    load_seq: u64,
    loaded_seq: u64,
    live_during_load: Vec<LogEvent>,
    history_sequence: u64,
}

pub struct LogDelta {
    pub epoch: i32,
    pub pending: Vec<LogEvent>,
    pub evicted: Vec<LogEvent>,
}

impl LogBuffer {
    pub fn logs(&self) -> &[LogEvent] {
        &self.logs
    }

    pub fn len(&self) -> usize {
        self.logs.len()
    }

    pub fn epoch(&self) -> i32 {
        self.epoch
    }

    pub fn begin_load(&mut self) -> u64 {
        self.load_seq = self.load_seq.wrapping_add(1);
        self.live_during_load.clear();
        self.load_seq
    }

    pub fn load_should_apply(&self, load_seq: u64) -> bool {
        load_seq == self.load_seq && load_seq > self.loaded_seq
    }

    pub fn complete_load(&mut self, load_seq: u64) {
        if load_seq == self.load_seq {
            self.loaded_seq = load_seq;
            self.live_during_load.clear();
        }
    }

    pub fn cancel_load(&mut self, load_seq: u64) {
        if load_seq == self.load_seq {
            self.loaded_seq = load_seq;
            self.live_during_load.clear();
        }
    }

    pub fn push(&mut self, log: LogEvent) {
        if log.sequence != 0 && log.sequence <= self.history_sequence {
            return;
        }
        if self.load_seq > self.loaded_seq {
            self.live_during_load.push(log.clone());
        }
        self.pending.push(log.clone());
        self.logs.push(log);
        if self.logs.len() > LOG_CAP {
            let excess = self.logs.len() - LOG_CAP;
            self.evicted.extend(self.logs.drain(..excess));
        }
    }

    pub fn take_delta(&mut self) -> Option<LogDelta> {
        if self.pending.is_empty() && self.evicted.is_empty() {
            return None;
        }
        Some(LogDelta {
            epoch: self.epoch,
            pending: std::mem::take(&mut self.pending),
            evicted: std::mem::take(&mut self.evicted),
        })
    }

    pub fn replace(&mut self, logs: Vec<LogEvent>) -> i32 {
        let live = std::mem::take(&mut self.live_during_load);
        let mut merged = logs;
        // A historical response may race with live events. Match duplicate
        // occurrences one-for-one, then retain events absent from the snapshot.
        // LogEvent contains the daemon timestamp, stream and exact message,
        // making it a stable identity for this local reconciliation.
        let mut matched = vec![false; merged.len()];
        for event in live {
            if let Some(index) = merged
                .iter()
                .enumerate()
                .find(|(index, candidate)| !matched[*index] && *candidate == &event)
                .map(|(index, _)| index)
            {
                matched[index] = true;
            } else {
                merged.push(event);
                matched.push(false);
            }
        }
        merged.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
        if merged.len() > LOG_CAP {
            merged = merged.split_off(merged.len() - LOG_CAP);
        }
        self.history_sequence = merged.iter().map(|event| event.sequence).max().unwrap_or(0);
        self.logs = merged;
        self.invalidate()
    }

    pub fn invalidate(&mut self) -> i32 {
        self.pending.clear();
        self.evicted.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }

    pub fn clear(&mut self) -> i32 {
        self.logs.clear();
        self.live_during_load.clear();
        self.history_sequence = 0;
        self.load_seq = self.load_seq.wrapping_add(1);
        self.loaded_seq = self.load_seq;
        self.invalidate()
    }
}

pub fn matches(log: &LogEvent, filter: &str, stream: &str) -> bool {
    let stream_matches = match stream {
        "stdout" => log.stream == LogStream::Stdout,
        "stderr" => log.stream == LogStream::Stderr,
        "system" => log.stream == LogStream::System,
        _ => true,
    };
    stream_matches && (filter.is_empty() || log.message.to_lowercase().contains(filter))
}

pub fn to_line(log: &LogEvent) -> LogLine {
    LogLine {
        time: log
            .timestamp
            .with_timezone(&Local)
            .format("%H:%M:%S")
            .to_string()
            .into(),
        level: match log.stream {
            LogStream::Stdout => "stdout",
            LogStream::Stderr => "stderr",
            LogStream::System => "system",
        }
        .into(),
        message: log.message.clone().into(),
    }
}

pub fn filtered_lines(logs: &[LogEvent], filter: &str, stream: &str) -> Vec<LogLine> {
    logs.iter()
        .filter(|log| matches(log, filter, stream))
        .map(to_line)
        .collect()
}

pub fn recent_tail(lines: &[LogLine]) -> Vec<LogLine> {
    lines[lines.len().saturating_sub(RECENT_CAP)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use server_on_core::config::ServiceId;

    fn event(index: i64, stream: LogStream, message: &str) -> LogEvent {
        LogEvent {
            service_id: ServiceId::from_string("service:test"),
            timestamp: DateTime::from_timestamp(1_700_000_000 + index, 0).unwrap(),
            stream,
            message: message.into(),
            sequence: index as u64 + 1,
        }
    }

    fn messages(lines: &[LogLine]) -> Vec<&str> {
        lines.iter().map(|line| line.message.as_str()).collect()
    }

    #[test]
    fn buffer_caps_history_and_tracks_evictions() {
        let mut buffer = LogBuffer::default();
        for index in 0..LOG_CAP + 5 {
            buffer.push(event(
                index as i64,
                LogStream::Stdout,
                &format!("line {index}"),
            ));
        }

        assert_eq!(buffer.len(), LOG_CAP);
        assert_eq!(buffer.logs()[0].message, "line 5");
        let delta = buffer.take_delta().unwrap();
        assert_eq!(delta.pending.len(), LOG_CAP + 5);
        assert_eq!(delta.evicted.len(), 5);
        assert!(buffer.take_delta().is_none());
    }

    #[test]
    fn filtered_delta_matches_rebuilding_the_canonical_buffer() {
        let mut buffer = LogBuffer::default();
        for index in 0..LOG_CAP {
            let stream = if index % 2 == 0 {
                LogStream::Stdout
            } else {
                LogStream::Stderr
            };
            buffer.push(event(index as i64, stream, &format!("Error {index}")));
        }
        let mut displayed = filtered_lines(buffer.logs(), "error", "stderr");
        buffer.take_delta();

        for index in LOG_CAP..LOG_CAP + 600 {
            let stream = if index % 2 == 0 {
                LogStream::Stdout
            } else {
                LogStream::Stderr
            };
            buffer.push(event(index as i64, stream, &format!("Error {index}")));
        }
        let delta = buffer.take_delta().unwrap();
        let removed = delta
            .evicted
            .iter()
            .filter(|log| matches(log, "error", "stderr"))
            .count();
        displayed.extend(filtered_lines(&delta.pending, "error", "stderr"));
        displayed.drain(..removed);

        assert_eq!(
            displayed.len(),
            filtered_lines(buffer.logs(), "error", "stderr").len()
        );
        assert_eq!(
            messages(&displayed),
            messages(&filtered_lines(buffer.logs(), "error", "stderr"))
        );
    }

    #[test]
    fn filtering_and_recent_tail_preserve_order() {
        let logs = vec![
            event(0, LogStream::Stdout, "Information"),
            event(1, LogStream::Stderr, "first ERROR"),
            event(2, LogStream::System, "second error"),
        ];
        let filtered = filtered_lines(&logs, "error", "all");
        assert_eq!(messages(&filtered), vec!["first ERROR", "second error"]);
        assert_eq!(filtered_lines(&logs, "error", "stderr").len(), 1);

        let lines = (0..40)
            .map(|index| LogLine {
                message: format!("line {index}").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let recent = recent_tail(&lines);
        assert_eq!(recent.len(), RECENT_CAP);
        assert_eq!(recent[0].message, "line 10");
        assert_eq!(recent[RECENT_CAP - 1].message, "line 39");
    }

    #[test]
    fn structural_changes_invalidate_queued_deltas() {
        let mut buffer = LogBuffer::default();
        buffer.push(event(0, LogStream::Stdout, "old"));
        let stale = buffer.take_delta().unwrap();
        let replacement_epoch = buffer.invalidate();
        assert_ne!(replacement_epoch, stale.epoch);

        buffer.push(event(1, LogStream::Stdout, "live"));
        assert_eq!(buffer.len(), 2);
        buffer.replace(Vec::new());
        assert_eq!(buffer.len(), 0);

        buffer.push(event(2, LogStream::Stdout, "queued"));
        let stale = buffer.take_delta().unwrap();
        buffer.clear();
        assert_ne!(buffer.epoch(), stale.epoch);
        assert!(buffer.logs().is_empty());
    }

    #[test]
    fn loads_only_apply_latest_request_until_clear() {
        let mut buffer = LogBuffer::default();
        let first = buffer.begin_load();
        buffer.invalidate();
        assert!(buffer.load_should_apply(first));

        let second = buffer.begin_load();
        assert!(!buffer.load_should_apply(first));
        assert!(buffer.load_should_apply(second));
        buffer.complete_load(first);
        assert!(buffer.load_should_apply(second));

        buffer.complete_load(second);
        assert!(!buffer.load_should_apply(first));
        assert!(!buffer.load_should_apply(second));
        buffer.clear();
        assert!(!buffer.load_should_apply(second));
    }

    #[test]
    fn history_load_preserves_live_events_and_deduplicates_overlap() {
        let mut buffer = LogBuffer::default();
        let load = buffer.begin_load();
        let duplicated = event(1, LogStream::Stdout, "already in history");
        let live = event(2, LogStream::Stderr, "arrived after the snapshot");
        buffer.push(duplicated.clone());
        buffer.push(live.clone());

        buffer.replace(vec![event(0, LogStream::System, "older"), duplicated]);
        buffer.complete_load(load);

        assert_eq!(buffer.len(), 3);
        assert_eq!(
            buffer
                .logs()
                .iter()
                .map(|log| log.message.as_str())
                .collect::<Vec<_>>(),
            ["older", "already in history", "arrived after the snapshot"]
        );
    }

    #[test]
    fn queued_events_already_covered_by_history_are_ignored_by_sequence() {
        let mut buffer = LogBuffer::default();
        buffer.replace(vec![
            event(0, LogStream::Stdout, "history-1"),
            event(1, LogStream::Stdout, "history-2"),
        ]);

        buffer.push(event(1, LogStream::Stdout, "queued duplicate"));
        buffer.push(event(2, LogStream::Stdout, "new live event"));

        assert_eq!(
            buffer
                .logs()
                .iter()
                .map(|log| log.message.as_str())
                .collect::<Vec<_>>(),
            ["history-1", "history-2", "new live event"]
        );
    }

    #[test]
    fn history_load_matches_duplicate_events_one_for_one() {
        let mut buffer = LogBuffer::default();
        buffer.begin_load();
        let duplicate = event(1, LogStream::Stdout, "same");
        buffer.push(duplicate.clone());
        buffer.push(duplicate.clone());
        buffer.push(duplicate.clone());

        buffer.replace(vec![duplicate.clone(), duplicate]);

        assert_eq!(buffer.len(), 3);
    }
}
