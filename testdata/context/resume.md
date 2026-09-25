# Alex Morgan

Senior Backend Engineer. Fictional person; this resume is a test fixture for `dmn sources`.

## Summary

Backend engineer with eight years of experience building and running distributed systems at
scale. Comfortable owning a service end to end: design, capacity planning, on-call and the
postmortem afterwards. Prefers boring technology, clear interfaces and measurable outcomes.

## Experience

### Staff Software Engineer, Northwind Payments (2021 to present)

- Designed a global rate limiter for the public API using token buckets in regional Redis
  clusters with a local in-process cache, cutting p99 latency of the limit check from 9 ms to
  under 1 ms while serving two billion requests a day.
- Led the migration of the ledger from a single Postgres primary to a sharded cluster keyed by
  merchant, with dual writes, backfill and a verified cutover that had zero data loss.
- Introduced service level objectives for checkout and an error-budget policy that the product
  and platform teams both signed off on; paging volume fell by half within two quarters.
- Mentored four engineers, two of whom were promoted; ran the backend interview loop.

### Senior Software Engineer, Contoso Logistics (2017 to 2021)

- Built the shipment event pipeline on Kafka, with idempotent consumers, a dead-letter topic
  and replay tooling, processing forty thousand events a second at peak.
- Rewrote the route-planning service from a nightly batch into a streaming job, so dispatchers
  saw updated estimates within a minute instead of the next morning.
- Owned the on-call rotation for three services and wrote the runbooks the team still uses.
- Reduced cloud spend by 30 percent by right-sizing clusters and moving cold data to object
  storage with lifecycle rules.

### Software Engineer, Fabrikam Studio (2015 to 2017)

- Shipped the first version of the public REST API and its OAuth flow.
- Added integration tests and a staging environment, which turned weekly releases into daily
  ones.

## Skills

Languages: Go, Rust, Python, SQL. Data: Postgres, Redis, Kafka, ClickHouse. Infrastructure:
Kubernetes, Terraform, AWS. Practices: service level objectives, incident review, capacity
planning, zero-downtime migrations.

## Education

BSc in Computer Science, State University, 2015.
