#!/usr/bin/env node
"use strict";

const {
  readPackageInfo,
  resolveExecutable,
  spawnResolvedExecutable,
} = require("../lib/windows-managed");

const resolved = resolveExecutable(readPackageInfo());
if (!resolved.exe) {
  const detail = resolved.installError ? ` (${resolved.installError.message})` : "";
  console.error(`rebon: could not find installed rebon.exe${detail}. Try reinstalling the npm package without disabling scripts.`);
  process.exit(1);
}

if (resolved.usedFallback) {
  const detail = resolved.installError ? `: ${resolved.installError.message}` : "";
  console.error(`rebon: using package payload fallback${detail}. Future npm upgrades may require closing this rebon process.`);
}

spawnResolvedExecutable(resolved, process.argv.slice(2), require("child_process").spawn);
