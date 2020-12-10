use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use derivative::Derivative;
use hazy::OpaqueDebug;
use libhoney::transmission::Transmission;
use libhoney::{Client, Event, FieldHolder, Sender, Value};
use log::{debug, error, warn};
use opentelemetry::exporter::trace::{ExportResult, SpanData, SpanExporter};
use opentelemetry::exporter::ExportError;
use opentelemetry::global::TracerProviderGuard;
use opentelemetry::sdk::Resource;
use opentelemetry::trace::{SpanId, StatusCode, TraceError, TraceId, TracerProvider};
use opentelemetry::{Array, KeyValue};
use thiserror::Error;

use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Produced by libhoney:
pub use libhoney::Error as HoneycombError;

/// Honeycomb API key.
///
/// The [`std::fmt::Debug`] implementation of this type redacts the key, so it is safe to embed in
/// other data structures.
#[derive(Clone, OpaqueDebug)]
pub struct HoneycombApiKey(String);
impl HoneycombApiKey {
    pub fn new(api_key: String) -> Self {
        Self(api_key)
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn secret(&self) -> &str {
        &self.0
    }
}

/// Create a new exporter pipeline builder.
pub fn new_pipeline(api_key: HoneycombApiKey, dataset: String) -> HoneycombPipelineBuilder {
    HoneycombPipelineBuilder {
        api_key,
        dataset,
        trace_config: None,
        transmission_options: libhoney::transmission::Options {
            user_agent_addition: Some(format!(
                "{}-rs/{}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            )),
            ..Default::default()
        },
    }
}

/// Pipeline builder
#[derive(Debug)]
pub struct HoneycombPipelineBuilder {
    api_key: HoneycombApiKey,
    dataset: String,
    trace_config: Option<opentelemetry::sdk::trace::Config>,
    transmission_options: libhoney::transmission::Options,
}
impl HoneycombPipelineBuilder {
    /// Assign the SDK trace configuration.
    pub fn with_trace_config(mut self, config: opentelemetry::sdk::trace::Config) -> Self {
        self.trace_config = Some(config);
        self
    }

    /// Sets the number of events to collect into a batch before sending to Honeycomb.
    pub fn with_max_batch_size(mut self, max_batch_size: usize) -> Self {
        self.transmission_options.max_batch_size = max_batch_size;
        self
    }

    /// Sets the number of batches that can be inflight to Honeycomb simultaneously.
    pub fn with_max_concurrent_batches(mut self, max_concurrent_batches: usize) -> Self {
        self.transmission_options.max_concurrent_batches = max_concurrent_batches;
        self
    }

    /// Specifies how often to send batches to Honeycomb.
    pub fn with_batch_timeout(mut self, batch_timeout: Duration) -> Self {
        self.transmission_options.batch_timeout = batch_timeout;
        self
    }

    /// Install the Honeycomb exporter pipeline with the recommended defaults.
    pub fn install(mut self) -> (opentelemetry::sdk::trace::Tracer, Uninstall) {
        let client = Arc::new(RwLock::new(libhoney::init(libhoney::Config {
            options: libhoney::client::Options {
                api_key: self.api_key.into_inner(),
                dataset: self.dataset,
                ..Default::default()
            },
            transmission_options: self.transmission_options,
        })));

        let exporter = HoneycombSpanExporter { client };

        // Libhoney implements its own batching, so we just use the simple exporter here instead of
        // double-batching (which would require double-flushing, etc.).
        let mut provider_builder =
            opentelemetry::sdk::trace::TracerProvider::builder().with_simple_exporter(exporter);
        if let Some(config) = self.trace_config.take() {
            provider_builder = provider_builder.with_config(config);
        }
        let provider = provider_builder.build();
        let tracer = provider.get_tracer(
            "opentelemetry-honeycomb-rs",
            Some(env!("CARGO_PKG_VERSION")),
        );
        let provider_guard = opentelemetry::global::set_tracer_provider(provider);

        (tracer, Uninstall(provider_guard))
    }
}

#[derive(Debug, Error)]
pub enum HoneycombExporterError {
    #[error("Honeycomb error")]
    Honeycomb(#[source] HoneycombError),
}
impl ExportError for HoneycombExporterError {
    fn exporter_name(&self) -> &'static str {
        "honeycomb"
    }
}

fn timestamp_from_system_time(ts: SystemTime) -> DateTime<Utc> {
    Utc.timestamp_millis(
        ts.duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64 as i64,
    )
}

fn otel_value_to_serde_json(value: opentelemetry::Value) -> Value {
    match value {
        opentelemetry::Value::Bool(val) => Value::Bool(val),
        opentelemetry::Value::I64(val) => Value::Number(val.into()),
        opentelemetry::Value::F64(val) => {
            if let Some(v) = serde_json::Number::from_f64(val) {
                Value::Number(v)
            } else {
                Value::Null
            }
        }
        opentelemetry::Value::Array(Array::Bool(vals)) => {
            Value::Array(vals.into_iter().map(Value::Bool).collect())
        }
        opentelemetry::Value::Array(Array::F64(vals)) => Value::Array(
            vals.into_iter()
                .flat_map(serde_json::Number::from_f64)
                .map(Value::Number)
                .collect(),
        ),
        opentelemetry::Value::Array(Array::I64(vals)) => {
            Value::Array(vals.into_iter().map(|v| Value::Number(v.into())).collect())
        }
        opentelemetry::Value::Array(Array::String(vals)) => Value::Array(
            vals.into_iter()
                .map(|v| Value::String(v.into_owned()))
                .collect(),
        ),
        opentelemetry::Value::String(val) => Value::String(val.into_owned()),
    }
}

/// Port of https://github.com/honeycombio/opentelemetry-exporter-python/blob/133ab6d7c5362ee24e4277264cf9a7634a5f6394/opentelemetry/ext/honeycomb/__init__.py#L36.
#[derive(Derivative)]
#[derivative(Debug)]
struct HoneycombSpanExporter {
    #[derivative(Debug = "ignore")]
    client: Arc<RwLock<Client<Transmission>>>,
}
impl HoneycombSpanExporter {
    fn new_trace_event<I>(
        client: &Client<Transmission>,
        start_time: SystemTime,
        trace_id: TraceId,
        parent_id: SpanId,
        attributes: I,
        resource: &Resource,
    ) -> Event
    where
        I: IntoIterator<Item = (opentelemetry::Key, opentelemetry::Value)>,
    {
        let mut event = client.new_event();
        let timestamp = timestamp_from_system_time(start_time);
        event.set_timestamp(timestamp);
        // The Honeycomb Python exporter sets start_time even though it seems to get ignored by
        // Honeycomb.
        event.add_field(
            "start_time",
            Value::String(timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
        event.add_field("trace.trace_id", Value::String(trace_id.to_hex()));

        // From the Honeycomb docs:
        //   "The root span for any given trace must have no field for “parentId” in its event. If
        //    all of the spans in a trace have a “parentId”, Honeycomb will not show a root span for
        //    that trace."
        if parent_id != SpanId::invalid() {
            event.add_field("trace.parent_id", Value::String(parent_id.to_hex()));
        }

        for (attr_name, attr_value) in resource
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .chain(attributes.into_iter())
        {
            event.add_field(attr_name.as_str(), otel_value_to_serde_json(attr_value))
        }

        event
    }
}

#[async_trait]
impl SpanExporter for HoneycombSpanExporter {
    async fn export(&mut self, batch: Vec<SpanData>) -> ExportResult {
        debug!("Exporting batch of {} spans", batch.len());
        let mut client = self
            .client
            .write()
            .expect("Honeycomb client lock is poisoned");
        for span in batch {
            {
                let mut event = Self::new_trace_event(
                    &client,
                    span.start_time,
                    span.span_context.trace_id(),
                    span.parent_span_id,
                    span.attributes,
                    &span.resource,
                );
                event.add_field(
                    "trace.span_id",
                    Value::String(span.span_context.span_id().to_hex()),
                );
                event.add_field("name", Value::String(span.name.clone()));
                if let Ok(duration_ms) = span.end_time.duration_since(span.start_time) {
                    event.add_field(
                        "duration_ms",
                        Value::Number((duration_ms.as_millis() as u64).into()),
                    );
                }
                event.add_field(
                    "response.status_code",
                    Value::Number((span.status_code.clone() as i32).into()),
                );
                event.add_field("status.message", Value::String(span.status_message.clone()));
                event.add_field("span.kind", Value::String(format!("{}", span.span_kind)));

                if !matches!(span.status_code, StatusCode::Unset) {
                    event.add_field(
                        "error",
                        Value::Bool(!matches!(span.status_code, StatusCode::Ok)),
                    );
                }

                event.send(&mut client).map_err(|err| {
                    TraceError::ExportFailed(Box::new(HoneycombExporterError::Honeycomb(err)))
                })?;
            }

            for span_event in span.message_events.into_iter() {
                let mut event = Self::new_trace_event(
                    &client,
                    span_event.timestamp,
                    span.span_context.trace_id(),
                    // The parent of the event is the current span, as opposed to the parent of the span,
                    // which is some other span (unless it's the root span).
                    span.span_context.span_id(),
                    span_event
                        .attributes
                        .into_iter()
                        .map(|KeyValue { key, value }| (key, value)),
                    &span.resource,
                );
                event.add_field("duration_ms", Value::Number(0.into()));
                event.add_field("name", Value::String(span_event.name));
                event.add_field(
                    "meta.annotation_type",
                    Value::String("span_event".to_string()),
                );

                event.send(&mut client).map_err(|err| {
                    TraceError::ExportFailed(Box::new(HoneycombExporterError::Honeycomb(err)))
                })?;
            }

            if !span.links.is_empty() {
                // FIXME: support links
                warn!("Links are not yet supported");
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        debug!("Shutting down HoneycombSpanExporter");
        // FIXME: ensure that all traces are flushed to Honeycomb before this function returns.
    }
}

impl Drop for HoneycombSpanExporter {
    fn drop(&mut self) {
        debug!("Closing Honeycomb client");
        self.client
            .write()
            .expect("Honeycomb client lock is poisoned")
            .transmission
            .stop()
            .unwrap_or_else(|err| error!("Failed to close Honeycomb client: {}", err))
    }
}

/// Uninstalls the Honeycomb pipeline on drop.
#[derive(Debug)]
pub struct Uninstall(TracerProviderGuard);
