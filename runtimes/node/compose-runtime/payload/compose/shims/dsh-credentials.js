// Embedded-runtime shim of `@deepseek-ai/dsh-credentials` (runtime surface
// only). The credentials *service* itself is the composition host's
// credentials-runtime.js seat; consumers reach it via `ctx.get('credentials')`.

const REF_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** Brand a raw string as a CredentialRef (a POSIX environment-variable name). */
export function credentialRef(value) {
  if (!REF_PATTERN.test(value)) {
    throw new TypeError(`credential ref "${value}" must match ${String(REF_PATTERN)}`);
  }
  return value;
}
