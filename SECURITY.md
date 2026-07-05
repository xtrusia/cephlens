# Security Policy

## Supported versions

cephlens is a lab-first prototype on the 0.x series. Only the latest release
receives fixes.

## Reporting a vulnerability

Report privately through GitHub's
["Report a vulnerability"](https://github.com/xtrusia/cephlens/security/advisories/new)
form rather than a public issue. Include the version, the affected component, and
steps to reproduce. Expect an initial response within about a week.

## Security model

cephlens installs no daemon and stores no credentials. It runs `ssh -o BatchMode=yes`
plus `sudo -n` against hosts you configure, and it starts eBPF tracers
(osdtrace / kfstrace / radostrace) that run as root on those hosts. It can only
reach hosts your SSH key and sudo rights already allow. The surfaces worth
scrutiny:

- Remote command construction from configured host names and binary paths.
- Parsing of untrusted tracer and Ceph CLI output.
- The cephtrace binaries bundled in release archives (see
  [`third_party/cephtrace`](third_party/cephtrace)).
