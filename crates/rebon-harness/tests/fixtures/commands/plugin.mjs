// A plugin that contributes slash commands and a service, and nothing else.
//
// The three command kinds a plugin may claim, one each:
//
//   * `prompt` — the only one with a handler, because it is the only one with
//     something to compute: rebon asks over `command/invoke` and what comes
//     back becomes the turn.
//   * `explain` — a sentence, said at registration. Nothing to ask for later.
//   * `panel`  — a dialog id the front end already knows how to open.
//
// The last one is `help`, which every build has a built-in for: it is here so
// the test can watch the seat refuse a name a built-in already answers to.

export function activate(api, config) {
  api.command(
    {
      name: "fixture-echo",
      description: "Echo the rest of the line back as a prompt",
      aliases: ["fx"],
      zhAliases: ["回声"],
      hint: "<text>",
      category: "command",
      surfaces: ["tui", "desktop"],
      kind: { type: "prompt" },
    },
    async (request, ctx) => {
      // Everything the seat knows about the invocation reaches the plugin:
      // the whole line, the part after the name, and where it was typed.
      return `fixture-echo(${request.surface}): ${request.rest} [${ctx.scopeId}]`;
    },
  );

  api.command({
    name: "fixture-elsewhere",
    description: "Say this command belongs on another surface",
    kind: { type: "explain", text: "Run /fixture-elsewhere in the desktop app." },
  });

  api.command({
    name: "fixture-panel",
    description: "Open the fixture panel",
    kind: { type: "panel", dialog: "fixture-dialog" },
  });

  // Only when asked for: the collision test loads a second copy of this
  // plugin with `takeHelp` set, so the happy path is not fighting a built-in
  // it did not mean to touch.
  if (config?.takeHelp) {
    api.command(
      { name: "help", description: "Not yours to take", kind: { type: "prompt" } },
      async () => "unreachable",
    );
  }

  // Never answers. The bound on a `command/invoke` is the only thing between
  // a plugin like this and a person watching a frozen screen.
  api.command(
    {
      name: "fixture-silent",
      description: "Never answers, on purpose",
      kind: { type: "prompt" },
    },
    () => new Promise(() => {}),
  );

  api.service("fixture-report", async (request) => ({
    saw: request?.method ?? null,
    params: request?.params ?? null,
  }));
}
