# Security

## Reporting a vulnerability

Report it privately, with **Report a vulnerability** on this repository's Security
tab, and not in a public issue, pull request or discussion. The report reaches the
maintainer only. Say what you found, how to reproduce it, and which version
(`faber --version`) it affects.

faber has a single maintainer, who answers as soon as they can. A fix is released
before the report is made public, and the advisory credits you if you wish.

## Supported versions

Only the latest release gets security fixes. A host that follows releases
([ADR-0006](docs/adrs/0006-fleet-autoupdate.md)) installs a fix by itself once it is
published and has waited out its maturity period.

## What matters most

faber runs CI jobs on its operator's own Macs, so these boundaries matter most:

- job code reaching the host outside the VM it runs in
  ([ADR-0026](docs/adrs/0026-job-code-runs-only-inside-vms-faber-owns.md));
- a job, its token or its author widening what faber trusts
  ([ADR-0014](docs/adrs/0014-the-trust-boundary-is-operator-configurable.md),
  [ADR-0030](docs/adrs/0030-the-gate-is-defense-in-depth-and-a-bypass-is-detected.md));
- a host installing a release that was not built from this repository's protected
  `main` ([ADR-0031](docs/adrs/0031-releases-are-signed-by-github-from-a-fresh-public-repository.md));
- the privacy gate letting a private name through
  ([ADR-0058](docs/adrs/0058-the-privacy-denylist-is-keyed.md)).
