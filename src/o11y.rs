use std::{env, time::Duration};

use anyhow::Result;
use opentelemetry::{
  sdk::{
    trace::{self, RandomIdGenerator, Sampler},
    Resource,
  },
  KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use tracing_subscriber::{filter::EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub fn setup() -> Result<()> {
  let t = tracing_subscriber::registry().with(tracing_subscriber::fmt::layer());

  if let Ok(tend) = env::var("TRACE_ENDPOINT") {
    let tracer = opentelemetry_otlp::new_pipeline()
      .tracing()
      .with_exporter(
        opentelemetry_otlp::new_exporter()
          .tonic()
          .with_endpoint(tend)
          .with_timeout(Duration::from_secs(3)),
      )
      .with_trace_config(
        trace::config()
          .with_sampler(Sampler::AlwaysOn)
          .with_id_generator(RandomIdGenerator::default())
          .with_resource(Resource::new(vec![KeyValue::new("service.name", "r5d3")])),
      )
      .install_batch(opentelemetry::runtime::Tokio)?;

    t.with(tracing_opentelemetry::layer().with_tracer(tracer))
      .with(EnvFilter::from_default_env())
      .init();
  } else {
    t.with(EnvFilter::from_default_env()).init();
  };

  Ok(())
}
