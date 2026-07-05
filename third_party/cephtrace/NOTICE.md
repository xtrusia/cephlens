# Bundled third-party components

cephlens release archives bundle the cephtrace tracers so that installation is
self-contained, including for air-gapped clusters. cephtrace is a separate
program: cephlens runs it over SSH and parses its output, and does not link
against it. Bundling it in the same archive is mere aggregation and does not
change cephlens's own MIT license.

## cephtrace

- Tools: `osdtrace`, `kfstrace`, `radostrace`
- License: GNU General Public License, version 2 (see `LICENSE` in this directory)
- Copyright: the cephtrace authors
- Source: https://github.com/taodd/cephtrace

The binaries are fetched unmodified from the official cephtrace GitHub releases
during the cephlens release build. The corresponding source is available at the
URL above, which satisfies GPL-2.0 section 3.
