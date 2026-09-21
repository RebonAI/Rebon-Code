# hello-command

The smallest package that puts something on Rebon's **kernel plugin plane**: one
JavaScript module exporting `activate`, one slash command, no services, no
tools, no provider. It exists so the plane's shape can be read in one sitting
and so the end-to-end tests have a package that is not a test fixture.

- `rebon-plugin.json` — declares `kernelPlugins.hello-command`, pointing at
  `plugin.mjs`, naming the one command the entry may register, the one seat it
  may call, and the one setting it owns.
- `plugin.mjs` — `activate(api, config)` registers `/hello`, a prompt command
  that answers with the plugin's `greeting` setting (falling back to
  `config.greeting`, when the composition sets one, and then to a fixed line).

## Rebon does not start the plane by itself

Out of the box no plugin plane runs. `kernelPlugins` is empty, no Node child
process starts, and nothing in this directory is loaded — the plane is
something a user turns on, not something a fresh install is already paying for.

To load this one, name it in `config.json` (`~/.rebon/config.json`, or a
project's):

```json
{
  "kernelPlugins": {
    "modules": {
      "hello-command": "runtimes/node/plugins/examples/hello-command/plugin.mjs"
    },
    "plugins": [{ "id": "hello-command" }]
  }
}
```

`modules` points at the module *file*; the package manifest beside it is what
says what the entry may register. `plugins` is the list that actually loads
things — a name mapped but not listed stays on disk.

Loading also needs a Node runtime in the supported range; `rebon node install`
provides one. With both in place, `/hello` appears in the command list and in
the `/` selector, and `/kernel` shows the entry running.

Installing the package (`rebon plugin install ./runtimes/node/plugins/examples/hello-command`)
is an alternative to the `modules` mapping: an installed package that declares a
name resolves under that name, so only the `plugins` list is then needed. Either
way that list is the switch — declaring a kernel plugin does not load it.

The composition can pass the entry a `config` object, which reaches `activate`
as its second argument:

```json
{ "id": "hello-command", "config": { "greeting": "Hallo" } }
```

## The setting it owns

Every plugin owns one namespace in `settings.json`: `plugins.<its id>`. The
keys in it are the ones the manifest declares, and this package declares one:

```json
"settings": [
  { "name": "greeting", "type": "string", "default": "Hello from the plugin plane" }
]
```

So a user changes what `/hello` says by putting it in their own settings, with
no composition involved:

```json
{ "plugins": { "hello-command": { "greeting": "Bonjour" } } }
```

The plugin reads it with `ctx.seat("settings", "read")` from inside the command
handler — a seat call is a request, so it needs a call to belong to, which is
why `activate`'s `api` has no `seat` and a handler's second argument does. The
answer arrives with the manifest's defaults already filled in, and merged down
the user → project → local chain per key. Reading needs `"settings"` in the
entry's `seats`.

Writing goes the other way and is fail-closed: `ctx.seat("settings", "write",
{ patch: { greeting: "Bonjour" } })` lands in the user's settings file, and a
key this manifest did not declare, the `enabled` switch beside it, or anybody
else's namespace is refused with a reason.

## What to copy from here

The declaration is a **ceiling**. Add a service, a tool or an event topic to
`activate` and the load fails until the same name appears in
`rebon-plugin.json`; that refusal is the point, not an inconvenience. See
The plugin manifest reference has the full field list and
why the plane is shaped this way.
