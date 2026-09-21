"use strict";

const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");

const INSTALL_SUPERVISOR_ENV = "REBON_INSTALL_SUPERVISOR";

function log(message) {
  console.log(`[rebon postinstall] ${message}`);
}

function envFlagEnabled(value) {
  return /^(1|true|yes|on)$/i.test(String(value || "").trim());
}

function supervisorHintMessage() {
  return "background supervisor scheduler was not registered; run `rebon agents service install` to keep queued jobs moving after login, or set REBON_INSTALL_SUPERVISOR=1 before install to opt in.";
}

function packageRootFromScript() {
  return path.resolve(__dirname, "..");
}

function installWindowsPayload(packageRoot) {
  const {
    installManagedPackage,
    readPackageInfo,
    requireManagedBinDir,
  } = require("../lib/windows-managed");

  requireManagedBinDir();
  const packageInfo = readPackageInfo(packageRoot);
  if (!fs.existsSync(packageInfo.payloadPath)) {
    throw new Error(`payload executable is missing: ${packageInfo.payloadPath}`);
  }

  const managedExe = installManagedPackage(packageInfo);
  if (!managedExe) {
    throw new Error("could not install payload executable into managed bin directory");
  }
  return managedExe;
}

function packageExecutable(packageRoot, platform) {
  if (platform === "win32") {
    return installWindowsPayload(packageRoot);
  }

  const exePath = path.join(packageRoot, "bin", "rebon");
  if (!fs.existsSync(exePath)) {
    throw new Error(`payload executable is missing: ${exePath}`);
  }

  try {
    fs.chmodSync(exePath, 0o755);
  } catch (_) {
  }
  return exePath;
}

function installSupervisorScheduler(exePath, spawnSyncImpl) {
  const result = spawnSyncImpl(exePath, ["agents", "service", "install"], {
    stdio: "inherit",
    windowsHide: true,
  });

  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`supervisor scheduler install exited with status ${result.status}`);
  }
}

function runPostinstall(options = {}) {
  const platform = options.platform || process.platform;
  const env = options.env || process.env;
  const logger = options.log || log;
  const spawnSyncImpl = options.spawnSync || childProcess.spawnSync;
  const packageRoot = options.packageRoot || packageRootFromScript();
  const exePath = packageExecutable(packageRoot, platform);

  logger(`installed ${exePath}`);

  if (envFlagEnabled(env[INSTALL_SUPERVISOR_ENV])) {
    logger(`registering per-user background supervisor scheduler because ${INSTALL_SUPERVISOR_ENV} is enabled`);
    installSupervisorScheduler(exePath, spawnSyncImpl);
    logger("registered background supervisor scheduler");
    return;
  }

  logger(supervisorHintMessage());
}

try {
  if (require.main === module) {
    runPostinstall();
  }
} catch (error) {
  console.error(`[rebon postinstall] ${error.message}`);
  process.exit(1);
}

module.exports = {
  INSTALL_SUPERVISOR_ENV,
  envFlagEnabled,
  installSupervisorScheduler,
  packageExecutable,
  runPostinstall,
  supervisorHintMessage,
};
