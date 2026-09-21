@echo off
setlocal

if not defined REBON_NPM_PACKAGE_NAME set "REBON_NPM_PACKAGE_NAME=@rebon/cli-win32-x64"

python "%~dp0build_npm_package.py" --build --package-name "%REBON_NPM_PACKAGE_NAME%" %*
