// Bundles the Basecoat runtime with the components stalias uses into one file.
import { readFileSync, writeFileSync } from 'node:fs';

const parts = ['basecoat', 'select', 'tabs', 'toast', 'sidebar'];
const source = parts
  .map((name) => readFileSync(`node_modules/basecoat-css/dist/js/${name}.min.js`, 'utf8'))
  .join(';\n');
writeFileSync('../src/web/static/basecoat.min.js', source + '\n');
console.log(`src/web/static/basecoat.min.js: ${parts.join(', ')}`);
