use std::time::Duration;

use anyhow::Result;
use aws_sdk_route53::types as rm;
use aws_config::Region;
use clap::Parser;
use control_aws::org::Account;
use tokio::time::sleep;
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
  let root_config = match app.root_role {
    Some(ref root_role) => control_aws::assume_role(root_role, None).await,
    None => aws_config::load_from_env().await,
  };

  let root_r53 = aws_sdk_route53::Client::new(&root_config);

  let rid = root_r53
    .list_hosted_zones()
    .send()
    .instrument(info_span!("list hosted zones on root domain"))
    .await?
    .hosted_zones()
    .into_iter()
    .find(|hz| hz.name().starts_with(&app.root_domain))
    .unwrap()
    .id()
    .to_string();

  info!(id = rid, "found root domain zone id");

  match control_aws::org::discover_accounts(root_config).await {
    Ok(accounts) => {
      for acc in accounts {
        let aid = acc.id.clone();
        work(app, acc, &root_r53, &rid)
          .instrument(info_span!("work on account", account = aid))
          .await
          .expect("failed to work on account");
      }
    }
    Err(e) => {
      println!("Failed to fetch accounts: {}", e);
      sleep(Duration::from_secs(fastrand::u64(60..300))).await;
    }
  }

  Ok(())
}

async fn work(app: &App, acc: Account, root_r53: &aws_sdk_route53::Client, rid: &String) -> Result<()> {
  if acc.environment.is_none() {
    info!(account = acc.id, "account has no environment tag, skipping");
    return Ok(());
  }

  let env = acc.environment.unwrap();

  let sub_role = format!("arn:aws:iam::{}:role{}", acc.id, app.discover_role);

  // ignore non-existing role
  let sts = aws_sdk_sts::Client::new(&control_aws::assume_role(&sub_role, None).await);
  match sts.get_caller_identity().send().await {
    Ok(_) => {
      info!(account = acc.id, environment = env, "successfully assumed role");
    }
    Err(e) => {
      debug!("ignore failed assume role: {:?}", e);
      return Ok(());
    }
  }

  let mut subdomains = Vec::new();

  let sub_r53 = aws_sdk_route53::Client::new(&control_aws::assume_role(&sub_role, None).await);
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

    let zname = zone.name();

    if zname != format!("{}.{}.", env, app.root_domain) {
      warn!(
        name = zone.name(),
        id = zone.id(),
        account = acc.id,
        environment = env,
        "found non-matching zone, skipping"
      );
      continue;
    }

    subdomains.push(zname.trim_end_matches('.').to_string());

    // first ensure the zone is delegated to the root domain
    let nsrr: Vec<_> = sub_r53
      .get_hosted_zone()
      .id(zone.id())
      .send()
      .instrument(info_span!("get subdomain delegation set", zone_name = zone.name()))
      .await?
      .delegation_set()
      .unwrap()
      .name_servers()
      .iter()
      .map(|ns| rm::ResourceRecord::builder().value(ns).build().unwrap())
      .collect();

    let cb = rm::ChangeBatch::builder()
      .changes(
        rm::Change::builder()
          .action(rm::ChangeAction::Upsert)
          .resource_record_set(
            rm::ResourceRecordSet::builder()
              .r#type(rm::RrType::Ns)
              .name(zname)
              .set_resource_records(Some(nsrr))
              .ttl(86400)
              .build()
              .unwrap(),
          )
          .build()
          .unwrap(),
      )
      .build()
      .unwrap();

    if app.dry_run {
      warn!("would upsert NS record: {:?}", &cb);
    } else {
      root_r53
        .change_resource_record_sets()
        .hosted_zone_id(rid)
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

      let sub_acm = aws_sdk_acm::Client::new(&control_aws::assume_role(&sub_role, Some(region)).await);

      let vals = acm::find_validations(sub_acm, &app.root_domain, &subdomains).await?;
      ret.extend(vals);
    }

    ret
  } else {
    let sub_acm = aws_sdk_acm::Client::new(&control_aws::assume_role(&sub_role, None).await);

    acm::find_validations(sub_acm, &app.root_domain, &subdomains).await?
  };

  for cb in cbs {
    if app.dry_run {
      warn!("would upsert DNS validation record: {:?}", &cb);
    } else {
      root_r53
        .change_resource_record_sets()
        .hosted_zone_id(rid)
        .change_batch(cb)
        .send()
        .instrument(info_span!("upsert DNS validation records"))
        .await?;
    }
  }

  Ok(())
}
