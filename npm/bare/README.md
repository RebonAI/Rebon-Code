# rebon

The Rebon agent CLI — an agent for coding and more. Install it globally:

```sh
npm install -g rebon
```

Then run:

```sh
rebon
```

## What this installs

`rebon` is a thin wrapper around **[`@rebon/cli`](https://www.npmjs.com/package/@rebon/cli)**.
Installing `rebon` pulls in `@rebon/cli` plus the matching prebuilt binary for
your platform, and forwards the `rebon` command to it. You can also install the
scoped package directly if you prefer:

```sh
npm install -g @rebon/cli@latest
```

Both give you the same `rebon` command.
