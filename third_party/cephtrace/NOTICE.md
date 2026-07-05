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
- Bundled release: `v1.6`

The binaries are fetched unmodified from the pinned cephtrace GitHub release
during the cephlens release build and verified before packaging:

| Tool | SHA256 |
| --- | --- |
| `osdtrace` | `ffee09562187a19bbf1ef4f0fe8de2acb820e90a1ebf3329521cc51757ffed6d` |
| `kfstrace` | `7aac74b5b19d9dbbc33e273fb284ba10f4ce3b07bca62216231b41014f734f31` |
| `radostrace` | `b7f63448ccac6e31a79e20af4142623ece67c2d5049bf84a3433749cfe8caa41` |

The corresponding cephtrace source for that release is available from the
source repository above. Redistribution of these bundled tools is governed by
GPL-2.0; cephlens itself remains MIT-licensed.
