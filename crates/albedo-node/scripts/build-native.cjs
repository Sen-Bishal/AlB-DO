const { spawnSync } = require('node:child_process');
const path = require('node:path');

const packageRoot = path.resolve(__dirname, '..');
const cargoTargetDir = path.join(packageRoot, 'target');
const extraArgs = process.argv.slice(2);
const executable =
  process.platform === 'win32'
    ? path.join(packageRoot, 'node_modules', '.bin', 'napi.cmd')
    : path.join(packageRoot, 'node_modules', '.bin', 'napi');

const result = spawnSync(executable, ['build', '--platform', ...extraArgs], {
  cwd: packageRoot,
  env: {
    ...process.env,
    CARGO_TARGET_DIR: cargoTargetDir,
  },
  shell: process.platform === 'win32',
  stdio: 'inherit',
});

if (result.error) {
  throw result.error;
}

process.exit(result.status === null ? 1 : result.status);
