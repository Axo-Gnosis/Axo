# axo

axo is a headless coding-agent harness core written in Rust. It follows pi's approach: a small core that stays small, with everything else built as extensions.

**Status:** design only. There is no code yet.

## What axo is

- **A Rust library first.** Hosts link the crates directly and run many sessions in one process.
- **A small headless binary second.** It speaks ACP over stdio, so editors and other programs can drive it.
- **Independent of any host.** Sworm is one host: it uses axo as a library and extends it in Rust. axo knows nothing about Sworm.

## What axo is not

- Not a terminal UI.
- Not a permission system.

## Docs

- [`docs/design.md`](docs/design.md): the big picture: goals, how axo is extended, providers and OAuth, open questions.
- [`docs/core.md`](docs/core.md): the core as a port of pi: crates, data model, loop, sessions, context, tools, decisions to make, and build order.
- [`docs/pi/`](docs/pi/README.md): detailed notes on pi's source that the port follows.
- [`docs/research/`](docs/research/README.md): the research behind these choices: alternatives considered, edit formats, durability.

## License

MIT; see [`LICENSE`](LICENSE). axo ports ideas and some text from [pi](https://github.com/earendil-works/pi) and [oh-my-pi](https://github.com/can1357/oh-my-pi), both MIT-licensed; code ported from them keeps their copyright notices.
