use anyhow::Result;
use aws_sdk_acm::types as am;
use aws_sdk_route53::types as rm;
use tracing::{info_span, instrument, Instrument};

#[instrument(skip_all)]
pub async fn find_validations(
  acm: aws_sdk_acm::Client,
  root_domain: &str,
  subdomains: &[String],
) -> Result<Vec<rm::ChangeBatch>> {
  let mut cbs = Vec::new();

  let mut certs = acm
    .list_certificates()
    .into_paginator()
    .items()
    .send()
    .instrument(info_span!("list ACM certificates"));

  while let Some(Ok(cert)) = certs.inner_mut().next().await {
    let val: Vec<_> = acm
      .describe_certificate()
      .certificate_arn(cert.certificate_arn().unwrap())
      .send()
      .instrument(info_span!(
        "describe certificate",
        arn = cert.certificate_arn().unwrap()
      ))
      .await?
      .certificate()
      .unwrap()
      .domain_validation_options()
      .iter()
      .cloned()
      .collect();

    for v in val {
      if v.validation_method() != Some(&am::ValidationMethod::Dns) {
        continue;
      }
      let domain = v.domain_name();
      if subdomains.iter().find(|s| domain.ends_with(*s)).is_none() && domain.ends_with(root_domain) {
        if let Some(rr) = v.resource_record() {
          let cb = rm::ChangeBatch::builder()
            .changes(
              rm::Change::builder()
                .action(rm::ChangeAction::Upsert)
                .resource_record_set(
                  rm::ResourceRecordSet::builder()
                    .r#type(rr.r#type().as_str().into())
                    .name(rr.name())
                    .resource_records(rm::ResourceRecord::builder().value(rr.value()).build().unwrap())
                    .ttl(86400)
                    .build()
                    .unwrap(),
                )
                .build()
                .unwrap(),
            )
            .build()
            .unwrap();

          cbs.push(cb);
        }
      }
    }
  }

  Ok(cbs)
}
