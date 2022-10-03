A controller for 'promoting' domains in AWS Route53. Scenario:

- You have a root domain managed in Route53 in account A - e.g. `example.com`
- You have various other accounts managing subdomains in Route53 - e.g. `foo.example.com` and `bar.example.com`
- You want to map a root domain record to a subdomain record - e.g. `x.example.com` should CNAME to `x.foo.example.com`

Normally, an operator (or CI) would need access to manage Route53 in multiple accounts to automate this. This controller alleviates these extra access which can be annoying and tricky to manage,

 Instead, this controller help to accomplish the same through a few tags on the subdomain records.

In addition, this controller also manages the Route53 zone itself (NS records on the root domain) and ACM certificates (DNS records for validation).

The controller can be configured with filters on both record level and subdomain level, which should help to minimize disruption by misconfiguration.

# Configuration

- TODO for each account to promote subdomains from, create IAM role with these policy and allow assume role


# Misc

The name is a tribute to [R2D2](https://en.wikipedia.org/wiki/R2-D2) by mashing together `Route53` and `DNS`.
