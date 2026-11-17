# Symphonia OGG Demuxer

[<img alt="Docs.rs" src="https://img.shields.io/badge/docs.rs-symphonia_format_ogg-brightgreen?style=for-the-badge" height="22"/>](https://docs.rs/symphonia-format-ogg)

OGG demuxer for Project Symphonia.

> [!NOTE]
> This crate is part of Symphonia. Please use the [`symphonia`](https://crates.io/crates/symphonia) crate instead of this one directly.

## Opus timing

Opus packet timestamps use the presentation timeline: granule positions are reduced by
the identification header's pre-skip, so encoder lead-in precedes time zero. Track
`start_ts` includes this lead-in, while the presented duration excludes pre-skip and
final padding. Packet start/end trims identify the samples to discard; `Track::delay`
is informational and must not be applied a second time. Each chained stream supplies
its own timing and signals the existing demuxer reset boundary.

## License

Symphonia is provided under the MPL v2.0 license. Please refer to the LICENSE file for more details.

## Contributing

Symphonia is a free and open-source project that welcomes contributions! To get started, please read our [Contribution Guidelines](https://github.com/pdeljanov/Symphonia/blob/main/CONTRIBUTING.md).
