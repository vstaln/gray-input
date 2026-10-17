<div align="center">
  <img alt="gray-input" src="assets/icon.svg" width="120" height="120" />
  <h1>gray-input</h1>
  <p><strong>One sidecar for everything that shapes what you submit.</strong></p>
  <p>
    <a href="https://gray.alignment.id">Website</a> ·
    <a href="https://github.com/vstaln/gray-input">Source</a> ·
    <a href="https://github.com/vstaln/gray">gray</a>
  </p>
  <p>
    <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-1c1c20?style=flat-square&labelColor=0a0a0b" /></a>
    <a href="https://www.rust-lang.org"><img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-rust-1c1c20?style=flat-square&labelColor=0a0a0b&logo=rust&logoColor=d4a373" /></a>
    <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-1c1c20?style=flat-square&labelColor=0a0a0b&color=7aa2f7" />
  </p>
</div>

<br/>

```bash
gray plugin install input
```

`gray-input` combines four input plugins into one sidecar process: prompt
templates, preference presets, input rewriting, and inline shell. Each
module keeps its own `~/.gray/<orig>/` state files and slash commands, so
existing setups keep working after you uninstall the standalone versions.

## Replaces

| Module | Former plugin | Hook | Commands | State |
|---|---|---|---|---|
| commands | `gray-commands` | — | `/cmd` | `~/.gray/commands/*.md` |
| preset | `gray-preset` | — | `/preset` | `~/.gray/presets/*.md` |
| transform | `gray-transform` | `input/submit` | `/transform` | `~/.gray/transform/rules.json` |
| inlinebash | `gray-inline-bash` | `input/submit` | `/inline` | `~/.gray/inline-bash/disabled` |

None of the modules registers tools. `/input` prints an overview of all four.
Capabilities are the union of the constituents: `host.say` (transform,
inline-bash) and `host.ask` (the `/cmd` and `/preset` pickers).

## `input/submit` chain

Submitted text passes through the modules in a fixed order:

```
commands → preset → transform → inline-bash
```

Each stage sees the text the previous stage produced. A `{text}` result
rewrites the input and passes it on. `{handled: true}` swallows the input and
stops the chain. `{}` passes the input through unchanged. commands and preset
have no `input/submit` hook upstream, so for now they always pass. Because
the chain is ordered, a transform rule can produce a shell line for
inline-bash. For example, `{"prefix":"?sh","rewrite":"!{rest}"}` makes
`?sh ls` run `ls` inline.

## Build from source

```sh
cargo build --release
gray plugin check ./target/release/input
```

## License

MIT
