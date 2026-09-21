// A package that declares a model provider and then registers nothing for it.
//
// Not a contrived shape: it is what a provider plugin looks like the moment
// its activate throws away a branch, renames the provider, or ships before
// the adapter does. The plane has to refuse it at load rather than hand back
// an adapter that answers nothing.
export function activate() {}
