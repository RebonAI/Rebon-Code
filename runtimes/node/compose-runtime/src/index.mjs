// The package's root export: the loader seam the plugin host runs with.
//
// `rebon-plugin-host --loader <this file>` is what turns a generic plugin host
// into one that can mount a Cordis composition. Everything else in this package
// hangs off that.
export { createLoader, cordisPluginOf } from './loader.mjs';
export { activate as composeControl } from './plugin.mjs';
