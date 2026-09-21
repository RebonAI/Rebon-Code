"use strict";

const fs = require("fs");
const path = require("path");

const CURRENT_MANIFEST = "current.json";
const EXECUTABLE_NAME = "rebon.exe";
const PAYLOAD_DIR = "payload";
const VERSIONS_DIR = "versions";
const STAGING_DIR = "staging";

function defaultPackageRoot() {
  return path.resolve(__dirname, "..");
}

function managedBinDir() {
  const base = process.env.LOCALAPPDATA || (process.env.USERPROFILE && path.join(process.env.USERPROFILE, ".rebon"));
  if (!base) {
    return null;
  }
  return path.join(base, "rebon", "bin");
}

function requireManagedBinDir() {
  const binDir = managedBinDir();
  if (!binDir) {
    throw new Error("LOCALAPPDATA and USERPROFILE are both unset; cannot choose a per-user install directory");
  }
  return binDir;
}

function sanitizeVersionPart(value) {
  return String(value || "unknown").replace(/[^0-9A-Za-z._-]/g, "_");
}

function firstString(candidates) {
  for (const candidate of candidates) {
    if (typeof candidate === "string" && candidate.length > 0) {
      return candidate;
    }
  }
  return null;
}

function packageBuildId(packageJson) {
  const rebon = packageJson && packageJson.rebon;
  return firstString([
    rebon && rebon.build,
    rebon && rebon.buildId,
    rebon && rebon.commit,
    rebon && rebon.hash,
    packageJson && packageJson.rebonBuild,
    packageJson && packageJson.rebonBuildId,
  ]);
}

function versionKeyFromPackageJson(packageJson) {
  const version = sanitizeVersionPart(packageJson && packageJson.version);
  const buildId = packageBuildId(packageJson);
  return buildId ? `${version}-${sanitizeVersionPart(buildId)}` : version;
}

function payloadSignature(payloadPath) {
  const stat = fs.statSync(payloadPath);
  return { size: stat.size, mtimeMs: Math.trunc(stat.mtimeMs) };
}

function readPackageInfo(packageRoot) {
  const root = path.resolve(packageRoot || defaultPackageRoot());
  const packageJson = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
  const payloadDir = path.join(root, PAYLOAD_DIR);
  const payloadPath = path.join(payloadDir, EXECUTABLE_NAME);
  return {
    packageRoot: root,
    name: String(packageJson.name || "rebon"),
    version: String(packageJson.version || "unknown"),
    versionKey: versionKeyFromPackageJson(packageJson),
    payloadDir,
    payloadPath,
    payload: fs.existsSync(payloadPath) ? payloadSignature(payloadPath) : null,
  };
}

function managedRelativeExecutable(versionKey) {
  return `${VERSIONS_DIR}/${versionKey}/${EXECUTABLE_NAME}`;
}

function payloadSpecificVersionKey(packageInfo) {
  if (!packageInfo.payload) {
    return packageInfo.versionKey;
  }
  return `${packageInfo.versionKey}-${packageInfo.payload.size}-${packageInfo.payload.mtimeMs}`;
}

function acceptedVersionKeys(packageInfo) {
  const keys = [packageInfo.versionKey];
  const payloadKey = payloadSpecificVersionKey(packageInfo);
  if (payloadKey !== packageInfo.versionKey) {
    keys.push(payloadKey);
  }
  return keys;
}

function layoutForVersion(binDir, versionKey) {
  const versionDir = path.join(binDir, VERSIONS_DIR, versionKey);
  const stagingDir = path.join(binDir, STAGING_DIR, versionKey);
  return {
    versionDir,
    stagingDir,
    exePath: path.join(versionDir, EXECUTABLE_NAME),
    manifestPath: path.join(binDir, CURRENT_MANIFEST),
    manifestRelativePath: managedRelativeExecutable(versionKey),
  };
}

function normalizeManifestPath(value) {
  return String(value || "").replace(/\\/g, "/").replace(/\/+/g, "/");
}

function resolveManifestPath(binDir, manifestPath) {
  if (typeof manifestPath !== "string" || path.isAbsolute(manifestPath)) {
    return null;
  }

  const resolved = path.resolve(binDir, manifestPath.replace(/[\\/]+/g, path.sep));
  const relative = path.relative(path.resolve(binDir), resolved);
  if (!relative || relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    return null;
  }
  return resolved;
}

function readCurrentManifest(binDir) {
  try {
    return JSON.parse(fs.readFileSync(path.join(binDir, CURRENT_MANIFEST), "utf8"));
  } catch (_) {
    return null;
  }
}

function payloadMatchesManifest(manifest, packageInfo) {
  if (!packageInfo.payload) {
    return true;
  }
  if (!manifest.payload) {
    return false;
  }
  return (
    Number(manifest.payload.size) === packageInfo.payload.size &&
    Math.trunc(Number(manifest.payload.mtimeMs)) === packageInfo.payload.mtimeMs
  );
}

function manifestMatchesPackage(manifest, binDir, packageInfo) {
  if (!manifest) {
    return false;
  }
  if (manifest.package !== packageInfo.name || manifest.version !== packageInfo.version) {
    return false;
  }
  if (!acceptedVersionKeys(packageInfo).includes(manifest.versionKey)) {
    return false;
  }
  if (normalizeManifestPath(manifest.path) !== managedRelativeExecutable(manifest.versionKey)) {
    return false;
  }

  const resolved = resolveManifestPath(binDir, manifest.path);
  return Boolean(resolved && fs.existsSync(resolved) && payloadMatchesManifest(manifest, packageInfo));
}

function readInstalledManifest(packageInfo) {
  const binDir = managedBinDir();
  if (!binDir) {
    return null;
  }

  const manifest = readCurrentManifest(binDir);
  if (!manifestMatchesPackage(manifest, binDir, packageInfo)) {
    return null;
  }
  return manifest;
}

function readInstalledExecutable(packageInfo) {
  const binDir = managedBinDir();
  const manifest = readInstalledManifest(packageInfo);
  if (!binDir || !manifest) {
    return null;
  }
  return resolveManifestPath(binDir, manifest.path);
}

function writeManifest(manifestPath, manifest) {
  const nonce = Math.random().toString(36).slice(2, 10);
  const tmpPath = `${manifestPath}.${process.pid}.${nonce}.tmp`;
  fs.mkdirSync(path.dirname(manifestPath), { recursive: true });
  fs.writeFileSync(tmpPath, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  fs.renameSync(tmpPath, manifestPath);
}

function copyPayloadDirectory(srcDir, destDir) {
  fs.mkdirSync(destDir, { recursive: true });
  const entries = fs.readdirSync(srcDir, { withFileTypes: true });
  for (const entry of entries) {
    const src = path.join(srcDir, entry.name);
    const dest = path.join(destDir, entry.name);
    if (entry.isDirectory()) {
      copyPayloadDirectory(src, dest);
    } else if (entry.isFile()) {
      fs.mkdirSync(path.dirname(dest), { recursive: true });
      fs.copyFileSync(src, dest);
    }
  }
}

function resetDirectory(dir) {
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch (_) {
  }
  fs.mkdirSync(dir, { recursive: true });
}

function removeDirectoryIfExists(dir) {
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch (_) {
  }
}

function cleanupDirectoryEntries(rootDir, keepName) {
  let entries;
  try {
    entries = fs.readdirSync(rootDir, { withFileTypes: true });
  } catch (_) {
    return;
  }

  for (const entry of entries) {
    if (!entry.isDirectory() || entry.name === keepName) {
      continue;
    }
    removeDirectoryIfExists(path.join(rootDir, entry.name));
  }
}

function cleanupLegacyRootFiles(binDir) {
  let entries;
  try {
    entries = fs.readdirSync(binDir, { withFileTypes: true });
  } catch (_) {
    return;
  }

  for (const entry of entries) {
    if (!entry.isFile()) {
      continue;
    }
    if (!/^rebon-.+\.exe$/i.test(entry.name)) {
      continue;
    }
    try {
      fs.unlinkSync(path.join(binDir, entry.name));
    } catch (_) {
    }
  }
}

function cleanupOldVersions(binDir, keepVersionKey) {
  cleanupDirectoryEntries(path.join(binDir, VERSIONS_DIR), keepVersionKey);
  cleanupDirectoryEntries(path.join(binDir, STAGING_DIR), keepVersionKey);
  cleanupLegacyRootFiles(binDir);
}

function promoteStagingDirectory(stagingDir, versionDir) {
  fs.mkdirSync(path.dirname(versionDir), { recursive: true });
  if (fs.existsSync(versionDir)) {
    fs.rmSync(versionDir, { recursive: true, force: true });
  }
  fs.renameSync(stagingDir, versionDir);
}

function installToVersionKey(packageInfo, binDir, versionKey) {
  const layout = layoutForVersion(binDir, versionKey);
  resetDirectory(layout.stagingDir);
  copyPayloadDirectory(packageInfo.payloadDir, layout.stagingDir);
  promoteStagingDirectory(layout.stagingDir, layout.versionDir);

  writeManifest(layout.manifestPath, {
    version: packageInfo.version,
    versionKey,
    path: layout.manifestRelativePath,
    package: packageInfo.name,
    payload: packageInfo.payload || payloadSignature(packageInfo.payloadPath),
    installedAt: new Date().toISOString(),
  });
  cleanupOldVersions(binDir, versionKey);
  return layout.exePath;
}

function installManagedPackage(packageInfo) {
  const binDir = managedBinDir();
  if (!binDir || !fs.existsSync(packageInfo.payloadPath)) {
    return null;
  }

  const installedManifest = readInstalledManifest(packageInfo);
  if (installedManifest) {
    cleanupOldVersions(binDir, installedManifest.versionKey);
    return resolveManifestPath(binDir, installedManifest.path);
  }

  try {
    return installToVersionKey(packageInfo, binDir, packageInfo.versionKey);
  } catch (error) {
    const fallbackVersionKey = payloadSpecificVersionKey(packageInfo);
    if (fallbackVersionKey === packageInfo.versionKey) {
      throw error;
    }
    return installToVersionKey(packageInfo, binDir, fallbackVersionKey);
  }
}

function resolveExecutable(packageInfo) {
  const info = packageInfo || readPackageInfo();
  const installed = readInstalledExecutable(info);
  if (installed) {
    return { exe: installed, usedFallback: false, installError: null };
  }

  let installError = null;
  try {
    const managed = installManagedPackage(info);
    if (managed) {
      return { exe: managed, usedFallback: false, installError: null };
    }
  } catch (error) {
    installError = error;
  }

  if (fs.existsSync(info.payloadPath)) {
    return { exe: info.payloadPath, usedFallback: true, installError };
  }

  return { exe: null, usedFallback: false, installError };
}

function spawnResolvedExecutable(resolved, argv, spawnImpl) {
  const child = spawnImpl(resolved.exe, argv, {
    stdio: "inherit",
    windowsHide: false,
    env: { ...process.env, REBON_CODE_MODE_NODE: process.execPath },
  });

  child.on("error", (error) => {
    console.error(`rebon: failed to start ${resolved.exe}: ${error.message}`);
    process.exit(1);
  });

  child.on("exit", (code, signal) => {
    if (signal) {
      try {
        process.kill(process.pid, signal);
      } catch (_) {
        process.exit(1);
      }
      return;
    }
    process.exit(code === null ? 1 : code);
  });

  return child;
}

module.exports = {
  cleanupOldVersions,
  installManagedPackage,
  layoutForVersion,
  managedBinDir,
  managedRelativeExecutable,
  payloadSignature,
  readCurrentManifest,
  readInstalledExecutable,
  readPackageInfo,
  requireManagedBinDir,
  resolveExecutable,
  spawnResolvedExecutable,
  versionKeyFromPackageJson,
};
