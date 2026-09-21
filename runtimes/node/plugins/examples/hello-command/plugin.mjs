// The smallest thing that can be a kernel plugin: one `activate`, one command.
//
// `activate` runs once, when the plane loads this entry. Everything a plugin
// contributes is registered here and nowhere else — the host takes the
// registrations made during this call and refuses anything that arrives
// afterwards, which is what makes "what did loading this add" a question with
// one answer.
//
// Every name registered has to appear in the manifest's declaration too
// (`commands: ["hello"]` in `rebon-plugin.json`). The declaration is a ceiling,
// not a request: registering a command absent from it fails the whole load
// rather than quietly dropping the command.

export function activate(api, config) {
  api.command(
    {
      name: "hello",
      description: "Say hello from the kernel plugin plane",
      // A prompt command's return value becomes the turn the model sees, the
      // same as if the text had been typed. `explain` and `panel` are the
      // other two kinds.
      kind: { type: "prompt" },
      // The closed set is tui / desktop / acp / web / mobile. Naming all five
      // is what makes this command appear everywhere; a narrower list is a
      // statement that the command does not work on the surfaces left out,
      // and each front end filters on it.
      surfaces: ["tui", "desktop", "acp", "web", "mobile"],
    },
    // The second argument is the call's own context, and it is the only thing
    // that reaches back into rebon. A seat is reached from here rather than
    // from `activate` because a seat call is a request, and a request needs a
    // call to belong to.
    async (request, ctx) => {
      // This plugin's own settings: `plugins.hello-command` in the user's
      // `settings.json`, whose keys are the ones `rebon-plugin.json` declares
      // under `settings`. Read per invocation, so changing the setting changes
      // the next `/hello` rather than the next session — and read with the
      // manifest's defaults already filled in, so a user who has never opened
      // their settings still gets a greeting.
      //
      // Reading is fail-soft: a kernel assembled without the seat is one this
      // plugin still works on, falling back to its config and then its default.
      // Writing is the fail-closed direction —
      // `ctx.seat("settings", "write", { patch: { greeting: "…" } })` is
      // refused for any key this manifest did not declare, for the `enabled`
      // switch, and for anybody else's namespace.
      let settings = {};
      try {
        settings = (await ctx.seat("settings", "read")) ?? {};
      } catch {
        // Nothing to tell the user: the greeting below still works.
      }
      const who =
        settings?.greeting ?? config?.greeting ?? "Hello from the plugin plane";
      const rest = request.rest.trim();
      return rest ? `${who}: ${rest}` : who;
    },
  );
}
