const fs = require('node:fs');
const path = require('node:path');

const packageRoot = path.resolve(__dirname, '..');
for (const entry of fs.readdirSync(packageRoot)) {
  if (entry.endsWith('.node')) {
    fs.rmSync(path.join(packageRoot, entry), { force: true });
  }
}
