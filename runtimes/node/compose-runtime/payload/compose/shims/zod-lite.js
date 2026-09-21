// Embedded-runtime shim of `zod` — construction surface only.
//
// Why it exists: vendored dsh plugins (tool-todo) build zod schema OBJECTS at
// module load for the session-projection seam (`ctx.sessionProjections`),
// which this composition never mounts — the schemas are constructed and then
// never consulted. Bundling real zod would add ~530KB of dead code per
// vendored plugin, so this shim satisfies module load with inert builders.
//
// Honesty boundary: NO validation semantics are provided. Any attempt to
// actually validate through a shimmed schema (`parse`/`safeParse`) throws
// loudly instead of silently accepting — if a projection consumer ever lands,
// replace this shim with a real vendored zod.

function inert(kind) {
  return {
    __zodLite: kind,
    parse() {
      throw new Error(
        'zod-lite shim: validation is not implemented (vendor real zod before consuming schemas)',
      );
    },
    safeParse() {
      throw new Error(
        'zod-lite shim: validation is not implemented (vendor real zod before consuming schemas)',
      );
    },
    optional() {
      return inert(`${kind}?`);
    },
    nullable() {
      return inert(`${kind}|null`);
    },
  };
}

export const z = {
  union: () => inert('union'),
  array: () => inert('array'),
  object: () => inert('object'),
  string: () => inert('string'),
  number: () => inert('number'),
  boolean: () => inert('boolean'),
  literal: () => inert('literal'),
  null: () => inert('null'),
  undefined: () => inert('undefined'),
  unknown: () => inert('unknown'),
};

export default z;
