//! The `tracing` → [`EventLog`] bridge. Spans are written once, on close, so
//! one line carries both what started them and how they ended.

use std::{sync::Mutex, time::Instant};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{layer::Context, registry::LookupSpan};

use crate::event_log::EventLog;

struct SpanState {
    fields: Map<String, Value>,
    opened: Instant,
}

#[derive(Default)]
struct JsonVisitor(Map<String, Value>);

impl Visit for JsonVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().into(), value.into());
    }
    /// `?value`, `%value` and the message, which `tracing` only exposes as `Debug`.
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}").into());
    }
}

/// Writes every event and every closed span as one JSON line.
pub struct EventLogLayer {
    log: Mutex<EventLog>,
    /// Stamped on every record: who wrote it (OpenTelemetry resource attributes).
    resource: Map<String, Value>,
    now: fn() -> DateTime<Utc>,
}

impl EventLogLayer {
    pub fn new(log: EventLog, resource: Map<String, Value>) -> Self {
        Self { log: Mutex::new(log), resource, now: Utc::now }
    }

    #[cfg(test)]
    fn with_clock(mut self, now: fn() -> DateTime<Utc>) -> Self {
        self.now = now;
        self
    }

    fn base(&self, kind: &str, meta: &tracing::Metadata<'_>, now: DateTime<Utc>) -> Map<String, Value> {
        let mut record = self.resource.clone();
        record.insert("ts".into(), now.to_rfc3339().into());
        record.insert("kind".into(), kind.into());
        record.insert("level".into(), meta.level().as_str().into());
        record.insert("target".into(), meta.target().into());
        record.insert("name".into(), meta.name().into());
        record
    }

    fn write(&self, record: Map<String, Value>, now: DateTime<Utc>) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.append(&Value::Object(record), now);
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S> for EventLogLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut visitor = JsonVisitor::default();
        attrs.record(&mut visitor);
        span.extensions_mut().insert(SpanState { fields: visitor.0, opened: Instant::now() });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut visitor = JsonVisitor::default();
        values.record(&mut visitor);
        if let Some(state) = span.extensions_mut().get_mut::<SpanState>() {
            state.fields.extend(visitor.0);
        }
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let now = (self.now)();
        let mut record = self.base("event", event.metadata(), now);
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        record.extend(visitor.0);
        self.write(record, now);
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(state) = span.extensions_mut().remove::<SpanState>() else { return };
        let now = (self.now)();
        let mut record = self.base("span", span.metadata(), now);
        record.insert("duration_ms".into(), (state.opened.elapsed().as_millis() as u64).into());
        record.extend(state.fields);
        self.write(record, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-26T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn capture(body: impl FnOnce()) -> Vec<Value> {
        // tracing's callsite-interest cache is global; captures run one at a time.
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let resource = Map::from_iter([("service.name".to_string(), Value::from("test"))]);
        let layer = EventLogLayer::new(EventLog::new(dir.path(), 7), resource).with_clock(fixed_now);
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), body);
        std::fs::read_to_string(dir.path().join("events-2026-10-26.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn event_carries_its_fields_and_resource() {
        let records = capture(|| tracing::info!(target: "tutor", action = "talk", on = true, "ui.action"));
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!((r["kind"].as_str(), r["level"].as_str(), r["message"].as_str()), (Some("event"), Some("INFO"), Some("ui.action")));
        assert_eq!((r["action"].as_str(), r["on"].as_bool()), (Some("talk"), Some(true)));
        assert_eq!(r["service.name"], "test");
        assert_eq!(r["ts"], "2026-10-26T12:00:00+00:00");
    }

    #[test]
    fn span_is_written_once_on_close_with_late_fields() {
        let records = capture(|| {
            let span = tracing::info_span!("deploy", host = "pc", outcome = tracing::field::Empty);
            span.in_scope(|| {});
            span.record("outcome", "copied");
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["name"], "deploy");
        assert_eq!(records[0]["outcome"], "copied");
        assert!(records[0]["duration_ms"].is_u64());
    }
}
