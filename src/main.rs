use std::{env, sync::Arc, time::Duration};

use anyhow::Result;
use aws_config::{default_provider::region::DefaultRegionChain, SdkConfig};
use aws_sdk_route53::types as rm;
use aws_types::region::Region;
use clap::Parser;
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tracing::{debug, info, info_span, instrument, warn, Instrument};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct App {
  /// run the controller without actually performing any modifications
  #[arg(long)]
  dry_run: bool,

  /// run the controller just once
  #[arg(long)]
  once: bool,

  /// root domain for the controller to manage
  #[arg(long)]
  root_domain: String,

  /// IAM role to assume for managing records on the root domain, will also discover all sub accounts in the organization. If not specificed, simply use the current environment
  #[arg(long)]
  root_role: Option<String>,

  #[arg(long)]
  /// IAM role (path + name) to (attempt to) assume in all sub accounts
  discover_role: String,

  /// AWS regions to check ACM certificates in. If not specified, only checks the default region
  #[arg(long = "region")]
  regions: Option<Vec<String>>,
}

mod acm;
mod o11y;

#[tokio::main]
async fn main() -> Result<()> {
  o11y::setup();

  let app = App::parse();
  debug!("loaded config: {:?}", app);

  loop {
    main_loop(&app).await?;

    if app.once {
      break;
    }

    sleep(Duration::from_secs(5 * 60)).await;
  }

  o11y::teardown();

  Ok(())
}

#[instrument(skip_all)]
async fn main_loop(app: &App) -> Result<()> {
  let default_region = DefaultRegionChain::builder()
    .build()
    .region()
    .instrument(info_span!("loading default region"))
    .await
    .unwrap();

  let root_r53 = {
    if let Some(root_role) = app.root_role.as_ref() {
      info!(root_role, "using root role");

      aws_sdk_route53::Client::new(&assume_role(root_role, default_region.clone()).await)
    } else {
      info!("no root role");
      let base_aws_config = aws_config::load_from_env().await;
      aws_sdk_route53::Client::new(&base_aws_config)
    }
  };

  let rid = root_r53
    .list_hosted_zones()
    .send()
    .instrument(info_span!("list hosted zones on root domain"))
    .await?
    .hosted_zones()
    .unwrap()
    .into_iter()
    .find(|hz| hz.name().unwrap().starts_with(&app.root_domain))
    .unwrap()
    .id()
    .unwrap()
    .to_string();

  info!(id = rid, "found root domain zone id");

  let accounts = discover_accounts(&app.root_role)
    .await
    .expect("failed to discover accounts");

  for acc in accounts {
    let span = info_span!("attempt to work on account", account = acc);
    let _guard = span.enter();

    let sub_role = format!("arn:aws:iam::{}:role{}", acc, app.discover_role);

    // ignore non-existing role
    let sts = aws_sdk_sts::Client::new(&assume_role(&sub_role, default_region.clone()).await);
    match sts.get_caller_identity().send().await {
      Ok(_) => {
        info!(account = acc, "successfully assumed role");
      }
      Err(e) => {
        debug!("ignore failed assume role: {:?}", e);
        continue;
      }
    }

    let mut subdomains = Vec::new();

    let sub_r53 = aws_sdk_route53::Client::new(&assume_role(&sub_role, default_region.clone()).await);
    let mut zones = sub_r53
      .list_hosted_zones()
      .into_paginator()
      .items()
      .send()
      .instrument(info_span!("fetch subdomain zones"));

    while let Some(Ok(zone)) = zones.inner_mut().next().await {
      // ignore private zones
      if zone.config().unwrap().private_zone() {
        debug!(name = zone.name(), id = zone.id(), "skipping private zone");
        continue;
      }

      subdomains.push(zone.name().unwrap().trim_end_matches('.').to_string());

      // first ensure the zone is delegated to the root domain
      let nsrr: Vec<_> = sub_r53
        .get_hosted_zone()
        .id(zone.id().unwrap())
        .send()
        .instrument(info_span!("get subdomain delegation set", zone_name = zone.name()))
        .await?
        .delegation_set()
        .unwrap()
        .name_servers()
        .unwrap()
        .iter()
        .map(|ns| rm::ResourceRecord::builder().value(ns).build())
        .collect();

      let cb = rm::ChangeBatch::builder()
        .changes(
          rm::Change::builder()
            .action(rm::ChangeAction::Upsert)
            .resource_record_set(
              rm::ResourceRecordSet::builder()
                .r#type(rm::RrType::Ns)
                .name(zone.name().unwrap())
                .set_resource_records(Some(nsrr))
                .ttl(86400)
                .build(),
            )
            .build(),
        )
        .build();

      if app.dry_run {
        warn!("would upsert NS record: {:?}", &cb);
      } else {
        root_r53
          .change_resource_record_sets()
          .hosted_zone_id(&rid)
          .change_batch(cb)
          .send()
          .instrument(info_span!("upsert NS record"))
          .await?;
      }
    }

    let cbs = if let Some(ref regions) = app.regions {
      let mut ret = Vec::new();
      for r_str in regions {
        let region = Region::new(r_str.clone());

        let sub_acm = aws_sdk_acm::Client::new(&assume_role(&sub_role, region).await);

        let vals = acm::find_validations(sub_acm, &app.root_domain, &subdomains).await?;
        ret.extend(vals);
      }

      ret
    } else {
      let sub_acm = aws_sdk_acm::Client::new(&assume_role(&sub_role, default_region.clone()).await);

      acm::find_validations(sub_acm, &app.root_domain, &subdomains).await?
    };

    for cb in cbs {
      if app.dry_run {
        warn!("would upsert DNS validation record: {:?}", &cb);
      } else {
        root_r53
          .change_resource_record_sets()
          .hosted_zone_id(&rid)
          .change_batch(cb)
          .send()
          .instrument(info_span!("upsert DNS validation records"))
          .await?;
      }
    }
  }

  Ok(())
}

async fn discover_accounts(root_role: &Option<String>) -> Result<Vec<String>> {
  let config = match root_role {
    Some(root_role) => {
      let region = DefaultRegionChain::builder()
        .build()
        .region()
        .instrument(info_span!("loading default region"))
        .await
        .expect("failed to load default region");

      assume_role(root_role, region).await
    }
    None => aws_config::load_from_env().await,
  };

  let org = aws_sdk_organizations::Client::new(&config);

  Ok(
    org
      .list_accounts()
      .send()
      .await?
      .accounts()
      .expect("failed to list accounts")
      .into_iter()
      .map(|a| a.id().expect("failed to extract account ID").to_string())
      .collect(),
  )
}

async fn assume_role(role: impl Into<String>, region: Region) -> SdkConfig {
  use aws_config::{default_provider::credentials::DefaultCredentialsChain, sts::AssumeRoleProvider};

  let provider = AssumeRoleProvider::builder(role)
    .session_name(env!("CARGO_PKG_NAME"))
    .region(region.clone())
    .build(Arc::new(DefaultCredentialsChain::builder().build().await) as Arc<_>);

  aws_config::from_env()
    .credentials_provider(provider)
    .region(region)
    .load()
    .await
}
