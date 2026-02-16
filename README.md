# Sundae Strategies

The Sundae v3 contracts support a special order type called a "Strategy".

A strategy order, rather than indicating a single specific point-in-time order, instead delegates trading authority to a public key. The owner of that public key can then decide, at a later date, what the specific details of the order should be, and sign that payload, giving it directly to the network of scoopers for execution.

This is a core protocol primitive that anyone can use, but to make it easy, we've built a suite of tools for making writing, running, and hosting those automated strategies very light weight from a developers perspective. This repository is that collection of tools.

It largely relies on [Balius](https://github.com/txpipe/balius) from TxPipe as a hosting environment for small webassembly files that can respond to events on the Cardano blockchain.

## Organization

- `balius-server` contains a small server for running strategies. Likely no longer needed, as you can use `baliusd` instead.
- `balius-worker-builder` is a small utility for compiling workers down into usable web assembly files.
- `sundae-strategies` is a crate you can depend on inside your balius workers. They provide utilities for writing strategies that reduce boilerplate significantly
- `workers` contains several example workers to draw inspiration from.

## Getting started

For setup instructions, see [QUICKSTART.md](./QUICKSTART.md).

When you're working on your strategies, you can use the Sundae SDK CLI to place a strategy order:

```sh
bunx @sundaeswap/cli
```

The best workflow is to run the worker once to initialize it's state, then stop baliusd. You can use

```sh
baliusd show-keys default
```

to show the public key, and then place the strategy order.

From there, you can run

```sh
baliusd --debug
```

While running in debug mode, it won't persist any state. Meaning if you stop baliusd and run it in debug mode again, it will replay all the same events, letting you iterate on your strategy as you get the logic right.

Please let us know if you have feedback on this development flow, we and the TxPipe team are always looking for opportunities to further streamline it!
