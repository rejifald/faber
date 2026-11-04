# Bundled fonts

The faces of [ADR-0040](../../../../docs/adrs/0040-icons-are-vendored-lucide-fonts-are-bundled-inter-and-plex-mono.md),
declared in [`../fonts.css`](../fonts.css). Both are under the SIL Open Font License 1.1. The
licence texts are in [`ui/public/licenses/`](../../../public/licenses/), which Vite copies into
every build, so each copy of a font ships with its licence. Nothing here updates on its own: a
new version is a PR that replaces the files and this table.

| File | Source | Made by |
| --- | --- | --- |
| `Inter-{Regular,Medium,SemiBold,Bold}.woff2` | [rsms/inter v4.1](https://github.com/rsms/inter/releases/tag/v4.1), `Inter-4.1.zip`, `web/` | subset here to Latin (below) with fontTools `pyftsubset`, all layout features kept |
| `IBMPlexMono-{Regular,Medium,SemiBold}.woff2` | [IBM/plex `@ibm/plex-mono@2.5.0`](https://github.com/IBM/plex/releases/tag/%40ibm%2Fplex-mono%402.5.0), `ibm-plex-mono.zip`, `fonts/split/woff2/*-Latin1.woff2` | IBM's own Latin 1 subset, unmodified, renamed without the `-Latin1` suffix |

**Why Plex is IBM's subset and Inter is ours.** Plex's licence reserves the font name
"Plex", and the OFL forbids a modified version from using a reserved name. A subset is a
modified version, so faber ships IBM's own subset files untouched. Inter reserves no name.

**Inter's subset** keeps the same code points as IBM's Latin 1 file, plus the arrows and
primes the UI writes:

```
U+0020-007E, U+00A0-00FF, U+0131, U+0152-0153, U+02C6, U+02DA, U+02DC, U+2013-2014,
U+2018-201A, U+201C-201E, U+2020-2022, U+2026, U+2030, U+2032-2033, U+2039-203A, U+2044,
U+20AC, U+2122, U+2190-2195, U+2212, U+2215, U+FB01-FB02
```

```sh
pyftsubset Inter-Regular.woff2 --unicodes="<the list above>" --layout-features='*' \
  --flavor=woff2 --output-file=Inter-Regular.woff2
```

A character outside the subset falls back to the next face in the stack, glyph by glyph.

| File | SHA-256 |
| --- | --- |
| `IBMPlexMono-Medium.woff2` | `41201b658a328b9d00368215c2f1102770f80b15952ab82631e4006255e6365d` |
| `IBMPlexMono-Regular.woff2` | `e8993d946649b9d01abb1ed06d574b19d8ea3e66b5c3948602db335c44c18e56` |
| `IBMPlexMono-SemiBold.woff2` | `b7acd05041ab65f3b7039e218ddd893065e11a07e85ea85019473152a51b6b7d` |
| `Inter-Bold.woff2` | `fa87cd107f1e8b7ef1a05618dd30ec3b570752a49e9834f59d2000c598394521` |
| `Inter-Medium.woff2` | `93a5c317843d6bf0c7fc2fd120f8d5021379b5f778773524e23faa00c2dcfd78` |
| `Inter-Regular.woff2` | `79eee72f51928c5922731f6ac05afcc81516cbb845d9861b7a72c4e101d27cde` |
| `Inter-SemiBold.woff2` | `7a4f72bb0f6e515e9365938ef7a42a1261373853924e6d5978e41e4d4a2635ca` |
