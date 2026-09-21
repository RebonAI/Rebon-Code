// Embedded-runtime shim of `@deepseek-ai/dsh-anonymous-user-id`.
//
// The real package persists a random id under the dsh home for telemetry
// correlation. The embedded runtime is not a dsh install and keeps no dsh
// home; a fixed marker keeps the (harmless, non-secret) telemetry header
// shape intact while identifying the traffic as rebon-embedded.

export function getOrCreateAnonymousUserId() {
  return 'rebon-embedded';
}
