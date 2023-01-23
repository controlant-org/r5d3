use std::{env, time::Duration};

use opentelemetry::{
  sdk::{
    trace::{self, RandomIdGenerator, Sampler},
    Resource,
  },
  KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use tracing_subscriber::{filter::EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub fn setup() {
  let otel_layer = env::var("OTLP_ENDPOINT").ok().map(|endpoint| {
    let tracer = opentelemetry_otlp::new_pipeline()
      .tracing()
      .with_exporter(
        opentelemetry_otlp::new_exporter()
          .tonic()
          .with_endpoint(endpoint)
          .with_timeout(Duration::from_secs(3)),
      )
      .with_trace_config(
        trace::config()
          .with_sampler(Sampler::AlwaysOn)
          .with_id_generator(RandomIdGenerator::default())
          .with_resource(Resource::new(vec![KeyValue::new("service.name", "r5d3")])),
      )
      .install_batch(opentelemetry::runtime::Tokio)
      .expect("failed to install otlp pipeline");

    tracing_opentelemetry::layer().with_tracer(tracer)
  });

  tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(otel_layer)
    .with(EnvFilter::from_default_env())
    .init();
}

pub fn teardown() {
  opentelemetry::global::shutdown_tracer_provider();
}
